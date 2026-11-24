//! Update check, download and verification. The daemon owns all of it so it can be tested; the
//! Android shell only installs a file this module has already verified.

use crate::app::{App, now};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub const DEFAULT_REPO: &str = "CalvinMagezi/kestrel";
const CHECK_EVERY_SECS: i64 = 12 * 3_600;
const FIRST_CHECK_DELAY_SECS: u64 = 20;
const APK_SUFFIX: &str = ".apk";
const SUMS_NAME: &str = "SHA256SUMS";
const READY_FILE: &str = "ready.json";
const APK_FILE: &str = "kestrel.apk";
/// Hosts a release download may come from, after redirects. GitHub serves assets from these.
const ALLOWED_HOSTS: &[&str] = &[
    "github.com",
    "api.github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];
const KV_ENABLED: &str = "update_enabled";
const KV_WIFI_ONLY: &str = "update_wifi_only";
const KV_DISMISSED: &str = "update_dismissed";
const MAX_NOTES_CHARS: usize = 1_200;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Release {
    pub version: String,
    pub notes: String,
    pub apk_url: String,
    pub sums_url: String,
    pub size: u64,
    pub page_url: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Idle,
    Checking,
    Available,
    Downloading,
    Ready,
    Error,
}

#[derive(Clone, Debug, Serialize)]
pub struct UpdateState {
    pub phase: Phase,
    pub release: Option<Release>,
    pub progress: u8,
    pub error: Option<String>,
    pub checked_at: i64,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            release: None,
            progress: 0,
            error: None,
            checked_at: 0,
        }
    }
}

/// `v1.2.3`, `1.2.3` or `1.2.3-rc1` as (1, 2, 3). Anything else is not a version.
pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let core = s.trim().trim_start_matches('v').split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u32>().ok());
    let v = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(v)
}

pub fn is_newer(current: &str, candidate: &str) -> bool {
    match (parse_version(current), parse_version(candidate)) {
        (Some(c), Some(n)) => n > c,
        _ => false,
    }
}

/// `owner/name` with only the characters GitHub allows, so it can sit safely in a URL.
pub fn valid_repo(repo: &str) -> bool {
    let ok = |p: &str| {
        !p.starts_with('.')
            && !p.is_empty()
            && p.len() <= 100
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    };
    repo.split_once('/')
        .is_some_and(|(o, n)| ok(o) && ok(n) && !n.contains('/'))
}

pub fn host_allowed(url: &str) -> bool {
    url.strip_prefix("https://")
        .and_then(|r| r.split(['/', ':', '?', '#']).next())
        .is_some_and(|h| ALLOWED_HOSTS.contains(&h))
}

/// Reads a GitHub "latest release" document. The release must carry one APK and a checksum file.
pub fn parse_release(v: &Value) -> Result<Release> {
    let tag = v["tag_name"].as_str().context("release has no tag")?;
    parse_version(tag).with_context(|| format!("tag {tag} is not a version"))?;
    let assets = v["assets"].as_array().context("release has no assets")?;
    let find =
        |pred: &dyn Fn(&str) -> bool| assets.iter().find(|a| a["name"].as_str().is_some_and(pred));
    let apk = find(&|n| n.ends_with(APK_SUFFIX)).context("release has no APK")?;
    let sums = find(&|n| n == SUMS_NAME).context("release has no SHA256SUMS file")?;
    let url = |a: &Value| a["browser_download_url"].as_str().map(str::to_string);
    let (apk_url, sums_url) = (
        url(apk).context("APK has no URL")?,
        url(sums).context("checksums have no URL")?,
    );
    if !host_allowed(&apk_url) || !host_allowed(&sums_url) {
        bail!("release assets are not on a trusted host");
    }
    let notes: String = v["body"]
        .as_str()
        .unwrap_or("")
        .chars()
        .take(MAX_NOTES_CHARS)
        .collect();
    Ok(Release {
        version: tag.trim_start_matches('v').to_string(),
        notes,
        apk_url,
        sums_url,
        size: apk["size"].as_u64().unwrap_or(0),
        page_url: v["html_url"].as_str().unwrap_or("").to_string(),
    })
}

/// The hash listed for `file` in a `sha256sum`-style text.
pub fn checksum_for(sums: &str, file: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (hash, name) = l.split_once(char::is_whitespace)?;
        let name = name.trim().trim_start_matches('*');
        (name == file && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

#[cfg(test)]
fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl App {
    fn update_dir(&self) -> String {
        format!("{}/update", self.cfg.data_dir)
    }

    fn flag(&self, key: &str, default: bool) -> bool {
        self.store
            .lock()
            .ok()
            .and_then(|s| s.get(key).ok().flatten())
            .map_or(default, |v| v == "1")
    }

    fn set_flag(&self, key: &str, on: bool) {
        if let Ok(s) = self.store.lock() {
            let _ = s.set(key, if on { "1" } else { "0" });
        }
    }

    fn set_update(&self, f: impl FnOnce(&mut UpdateState)) {
        if let Ok(mut st) = self.update.lock() {
            f(&mut st);
        }
    }

    /// Asks the release feed for the newest version and records whether it is newer than ours.
    pub async fn check_update(&self) -> Result<()> {
        anyhow::ensure!(
            valid_repo(&self.cfg.update_repo),
            "update repository is not valid"
        );
        self.set_update(|s| s.phase = Phase::Checking);
        let url = format!(
            "https://api.github.com/repos/{}/releases/latest",
            self.cfg.update_repo
        );
        let result = self.fetch_release(&url).await;
        let t = now();
        match result {
            Ok(Some(rel)) if is_newer(&self.cfg.app_version, &rel.version) => {
                let first_time =
                    self.update.lock().ok().is_none_or(|s| {
                        s.release.as_ref().map(|r| &r.version) != Some(&rel.version)
                    });
                let ready = self.ready_version().is_some_and(|v| v == rel.version);
                self.set_update(|s| {
                    s.checked_at = t;
                    s.error = None;
                    s.release = Some(rel.clone());
                    if ready {
                        s.phase = Phase::Ready;
                        s.progress = 100;
                    } else if s.phase != Phase::Downloading {
                        s.phase = Phase::Available;
                    }
                });
                if first_time {
                    self.journal("update", &format!("Kestrel {} is available", rel.version));
                }
                Ok(())
            }
            Ok(_) => {
                self.set_update(|s| {
                    *s = UpdateState {
                        checked_at: t,
                        ..UpdateState::default()
                    }
                });
                let _ = std::fs::remove_dir_all(self.update_dir());
                Ok(())
            }
            Err(e) => {
                let msg = e.to_string();
                self.set_update(|s| {
                    s.checked_at = t;
                    s.phase = if s.release.is_some() {
                        Phase::Available
                    } else {
                        Phase::Idle
                    };
                    s.error = Some(msg);
                });
                Err(e)
            }
        }
    }

    async fn fetch_release(&self, url: &str) -> Result<Option<Release>> {
        let resp = self
            .http
            .get(url)
            .header("User-Agent", "kestrel-update")
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let resp = resp.error_for_status()?;
        let bytes = resp.bytes().await?;
        self.record_bytes(bytes.len());
        parse_release(&serde_json::from_slice(&bytes)?).map(Some)
    }

    /// Downloads the APK, checks it against the published checksum, and marks it ready.
    pub async fn download_update(self: &Arc<Self>) -> Result<()> {
        let rel = self
            .update
            .lock()
            .ok()
            .and_then(|s| s.release.clone())
            .context("no update to download")?;
        self.set_update(|s| {
            s.phase = Phase::Downloading;
            s.progress = 0;
            s.error = None;
        });
        let outcome = self.download_verified(&rel).await;
        match &outcome {
            Ok(()) => self.set_update(|s| {
                s.phase = Phase::Ready;
                s.progress = 100;
            }),
            Err(e) => {
                let msg = e.to_string();
                self.set_update(|s| {
                    s.phase = Phase::Error;
                    s.error = Some(msg);
                });
            }
        }
        outcome
    }

    async fn download_verified(&self, rel: &Release) -> Result<()> {
        let dir = self.update_dir();
        std::fs::create_dir_all(&dir)?;
        let _ = std::fs::remove_file(format!("{dir}/{READY_FILE}"));
        let sums = self
            .http
            .get(&rel.sums_url)
            .header("User-Agent", "kestrel-update")
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        self.record_bytes(sums.len());
        let apk_name = rel.apk_url.rsplit('/').next().unwrap_or(APK_FILE);
        let expected =
            checksum_for(&sums, apk_name).with_context(|| format!("no checksum for {apk_name}"))?;
        let mut resp = self
            .http
            .get(&rel.apk_url)
            .header("User-Agent", "kestrel-update")
            .send()
            .await?
            .error_for_status()?;
        if !host_allowed(resp.url().as_str()) {
            bail!("download redirected to an untrusted host");
        }
        let total = resp.content_length().unwrap_or(rel.size).max(1);
        let part = format!("{dir}/{APK_FILE}.part");
        let mut file = std::fs::File::create(&part)?;
        let mut hasher = Sha256::new();
        let mut done: u64 = 0;
        while let Some(chunk) = resp.chunk().await? {
            std::io::Write::write_all(&mut file, &chunk)?;
            hasher.update(&chunk);
            done += chunk.len() as u64;
            let pct = (done * 100 / total).min(99) as u8;
            self.set_update(|s| s.progress = pct);
        }
        file.sync_all()?;
        self.record_bytes(done as usize);
        let got: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if got != expected {
            let _ = std::fs::remove_file(&part);
            bail!("the download did not match its checksum, so it was discarded");
        }
        std::fs::rename(&part, format!("{dir}/{APK_FILE}"))?;
        let marker = json!({"version": rel.version, "sha256": got, "size": done}).to_string();
        std::fs::write(format!("{dir}/{READY_FILE}"), marker)?;
        Ok(())
    }

    fn ready_version(&self) -> Option<String> {
        let text = std::fs::read_to_string(format!("{}/{READY_FILE}", self.update_dir())).ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        v["version"].as_str().map(str::to_string)
    }

    pub fn update_json(&self) -> Value {
        let st = self.update.lock().map(|s| s.clone()).unwrap_or_default();
        let dismissed = self
            .store
            .lock()
            .ok()
            .and_then(|s| s.get(KV_DISMISSED).ok().flatten());
        let hidden = st
            .release
            .as_ref()
            .is_some_and(|r| dismissed.as_deref() == Some(r.version.as_str()));
        json!({
            "phase": st.phase, "current": self.cfg.app_version, "release": st.release,
            "progress": st.progress, "error": st.error, "checked_at": st.checked_at,
            "enabled": self.flag(KV_ENABLED, true), "wifi_only": self.flag(KV_WIFI_ONLY, true),
            "dismissed": hidden,
        })
    }

    /// Runs one update action from the API and returns the new state.
    pub async fn update_action(self: &Arc<Self>, body: &Value) -> Result<Value, String> {
        match body["action"].as_str().unwrap_or("") {
            "check" => {
                let app = self.clone();
                tokio::spawn(async move {
                    let _ = app.check_update().await;
                });
            }
            "download" => {
                let busy = self
                    .update
                    .lock()
                    .map(|s| s.phase == Phase::Downloading)
                    .unwrap_or(false);
                if !busy {
                    let app = self.clone();
                    tokio::spawn(async move {
                        let _ = app.download_update().await;
                    });
                }
            }
            "dismiss" => {
                let v = self
                    .update
                    .lock()
                    .ok()
                    .and_then(|s| s.release.as_ref().map(|r| r.version.clone()));
                if let (Some(v), Ok(s)) = (v, self.store.lock()) {
                    let _ = s.set(KV_DISMISSED, &v);
                }
            }
            "set" => {
                if let Some(on) = body["enabled"].as_bool() {
                    self.set_flag(KV_ENABLED, on);
                }
                if let Some(on) = body["wifi_only"].as_bool() {
                    self.set_flag(KV_WIFI_ONLY, on);
                }
            }
            other => return Err(format!("unknown action {other:?}")),
        }
        Ok(self.update_json())
    }

    pub fn update_enabled(&self) -> bool {
        self.flag(KV_ENABLED, true)
    }
}

/// Checks once shortly after start and then every twelve hours while checking is on.
pub async fn update_loop(app: Arc<App>) {
    tokio::time::sleep(std::time::Duration::from_secs(FIRST_CHECK_DELAY_SECS)).await;
    loop {
        let due = app
            .update
            .lock()
            .map(|s| now() - s.checked_at >= CHECK_EVERY_SECS)
            .unwrap_or(true);
        if due && app.update_enabled() {
            let _ = app.check_update().await;
        }
        tokio::time::sleep(std::time::Duration::from_secs(600)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(tag: &str) -> Value {
        json!({
            "tag_name": tag, "html_url": "https://github.com/o/r/releases/tag/x", "body": "Fixes and a new bar.",
            "assets": [
                {"name": "Kestrel-android.apk", "size": 12345, "browser_download_url": "https://github.com/o/r/releases/download/v0.2.0/Kestrel-android.apk"},
                {"name": "SHA256SUMS", "size": 90, "browser_download_url": "https://github.com/o/r/releases/download/v0.2.0/SHA256SUMS"}
            ]
        })
    }

    #[test]
    fn versions_parse_and_compare() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.10.0-rc1"), Some((0, 10, 0)));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("latest"), None);
        assert!(is_newer("0.1.0", "v0.1.1"));
        assert!(is_newer("0.9.0", "0.10.0"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.2.0", "0.1.9"));
        assert!(!is_newer("0.2.0", "nightly"));
    }

    #[test]
    fn a_release_needs_an_apk_a_checksum_file_and_trusted_hosts() {
        let r = parse_release(&sample("v0.2.0")).unwrap();
        assert_eq!((r.version.as_str(), r.size), ("0.2.0", 12345));
        let mut no_sums = sample("v0.2.0");
        no_sums["assets"].as_array_mut().unwrap().pop();
        assert!(
            parse_release(&no_sums)
                .unwrap_err()
                .to_string()
                .contains("SHA256SUMS")
        );
        let mut evil = sample("v0.2.0");
        evil["assets"][0]["browser_download_url"] =
            json!("https://evil.example/Kestrel-android.apk");
        assert!(
            parse_release(&evil)
                .unwrap_err()
                .to_string()
                .contains("trusted host")
        );
        assert!(parse_release(&sample("nightly")).is_err());
    }

    #[test]
    fn hosts_and_repos_are_checked_strictly() {
        assert!(host_allowed("https://github.com/a/b"));
        assert!(host_allowed("https://objects.githubusercontent.com/x?y=1"));
        assert!(!host_allowed("http://github.com/a"));
        assert!(!host_allowed("https://github.com.evil.example/a"));
        assert!(!host_allowed("https://evil.example/github.com"));
        assert!(valid_repo("CalvinMagezi/kestrel"));
        assert!(!valid_repo("a/b/c"));
        assert!(!valid_repo("../etc"));
        assert!(!valid_repo("a/b?x=1"));
    }

    #[test]
    fn checksums_are_read_from_a_sums_file_and_hashing_matches() {
        let h = sha256_hex(b"kestrel");
        let sums = format!("{h}  Kestrel-android.apk\n{}  other.zip\n", "0".repeat(64));
        assert_eq!(
            checksum_for(&sums, "Kestrel-android.apk").as_deref(),
            Some(h.as_str())
        );
        assert_eq!(checksum_for(&sums, "missing.apk"), None);
        assert_eq!(
            checksum_for("abc  Kestrel-android.apk", "Kestrel-android.apk"),
            None
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
