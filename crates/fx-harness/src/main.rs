//! `fxh`: forex paper-trading research harness for one user.

mod api;
mod app;
mod chat;
mod update;
mod web;

use anyhow::{Context, Result};
use app::{App, Config};
use fx_core::Timeframe;
use std::sync::Arc;

const DEFAULT_PORT: u16 = 8787;
const DEFAULT_PAIRS: &str = "EURUSD,GBPUSD,USDJPY";
const DEFAULT_MODEL: &str = "gemma4:e2b";
const TOKEN_BYTES: usize = 16;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn parse_tf(s: &str) -> Result<Timeframe> {
    match s.to_ascii_uppercase().as_str() {
        "M15" => Ok(Timeframe::M15),
        "H1" => Ok(Timeframe::H1),
        "H4" => Ok(Timeframe::H4),
        "D1" => Ok(Timeframe::D1),
        other => anyhow::bail!("unknown timeframe {other}, use M15, H1, H4 or D1"),
    }
}

fn is_valid_token(t: &str) -> bool {
    t.len() == TOKEN_BYTES * 2 && t.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Writes `contents` to `path` through a temporary file, readable only by the owner.
fn write_private(path: &str, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = format!("{path}.tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn load_or_make_token(dir: &str) -> Result<String> {
    let path = format!("{dir}/token");
    if let Ok(t) = std::fs::read_to_string(&path)
        && is_valid_token(t.trim())
    {
        return Ok(t.trim().to_string());
    }
    let mut bytes = [0u8; TOKEN_BYTES];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut bytes)?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    write_private(&path, &token)?;
    Ok(token)
}

/// The key comes from a private file in the data directory or the environment, never from the
/// command line, where any process on the device could read it.
fn load_anthropic_key(dir: &str) -> Option<String> {
    let from_file = std::fs::read_to_string(format!("{dir}/anthropic.key")).ok();
    let key = from_file.or_else(|| std::env::var("FX_ANTHROPIC_KEY").ok())?;
    let key = key.trim().to_string();
    (!key.is_empty()).then_some(key)
}

/// Keeps the pairs the harness can size and value, and says so if none are left.
fn supported_pairs(list: &str) -> Result<Vec<String>> {
    let pairs: Vec<String> = list
        .split(',')
        .filter_map(|p| fx_core::pair_spec(p.trim()).map(|s| s.name.to_string()))
        .collect();
    anyhow::ensure!(!pairs.is_empty(), "no supported pair in --pairs {list}");
    Ok(pairs)
}

fn config(args: &[String]) -> Result<Config> {
    let data_dir = arg(args, "--data-dir").unwrap_or_else(|| "./fx-data".into());
    std::fs::create_dir_all(&data_dir).with_context(|| format!("cannot create {data_dir}"))?;
    Ok(Config {
        port: arg(args, "--port")
            .map_or(Ok(DEFAULT_PORT), |p| p.parse())
            .context("bad --port")?,
        tf: parse_tf(&arg(args, "--tf").unwrap_or_else(|| "H1".into()))?,
        pairs: supported_pairs(&arg(args, "--pairs").unwrap_or_else(|| DEFAULT_PAIRS.into()))?,
        token: load_or_make_token(&data_dir)?,
        anthropic_key: load_anthropic_key(&data_dir),
        ollama_url: arg(args, "--ollama"),
        ollama_model: arg(args, "--ollama-model").unwrap_or_else(|| DEFAULT_MODEL.into()),
        update_repo: arg(args, "--update-repo").unwrap_or_else(|| update::DEFAULT_REPO.into()),
        app_version: arg(args, "--app-version").unwrap_or_else(|| env!("CARGO_PKG_VERSION").into()),
        data_dir,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = config(&args)?;
    let port_file = format!("{}/port", cfg.data_dir);
    let _ = std::fs::remove_file(&port_file);
    let addr = format!("127.0.0.1:{}", cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("cannot listen on {addr}"))?;
    // Port 0 asks the system for a free port, so another app cannot sit on a well-known one.
    let port = listener.local_addr()?.port();
    write_private(&port_file, &port.to_string())?;
    let app = Arc::new(App::new(cfg)?);
    println!(
        "fxh listening on 127.0.0.1:{port}, token in {}/token",
        app.cfg.data_dir
    );
    tokio::spawn(app::scheduler(app.clone()));
    tokio::spawn(update::update_loop(app.clone()));
    axum::serve(listener, web::router(app)).await?;
    Ok(())
}
