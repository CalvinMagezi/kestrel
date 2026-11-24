//! Shared state and the closed-candle scheduler.

use anyhow::Result;
use fx_core::backtest::group_by_known_at;
use fx_core::detect::analyze;
use fx_core::risk::RiskConfig;
use fx_core::sim::{Sim, SimConfig};
use fx_core::strategy::{Driver, StrategyConfig};
use fx_core::{Candle, Timeframe, pair_spec};
use fx_feed::{Store, yahoo};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const HISTORY_BARS: usize = 3_000;
pub const START_EQUITY: f64 = 10_000.0;
const MIN_POLL_SECS: i64 = 60;
const POLL_FRACTION: i64 = 3;
pub const TICK_SECS: u64 = 30;
const UGX_MAX_AGE_SECS: i64 = 6 * 3_600;
const FEED_FAILS_BEFORE_ALERT: u32 = 2;
/// Candles re-fetched behind the newest stored one, because Yahoo may revise the last few.
const TAIL_REFETCH_BARS: i64 = 6;
const JOURNAL_KEEP: usize = 20_000;
/// A stored history older than this is refilled in full, so a long outage leaves no hole.
const FULL_REFETCH_GAP_SECS: i64 = 4 * 86_400;
const PRUNE_EVERY_TICKS: u64 = 120;

pub struct Config {
    pub data_dir: String,
    pub port: u16,
    pub tf: Timeframe,
    pub pairs: Vec<String>,
    pub token: String,
    pub anthropic_key: Option<String>,
    pub ollama_url: Option<String>,
    pub ollama_model: String,
    /// `owner/name` of the repository whose releases carry updates.
    pub update_repo: String,
    /// The installed version, so the update check knows what counts as newer.
    pub app_version: String,
}

#[derive(Serialize, Deserialize)]
pub struct PairState {
    pub driver: Driver,
    pub last_ts: i64,
    #[serde(skip)]
    pub last_poll: i64,
    #[serde(skip)]
    pub feed_error: Option<String>,
}

pub struct App {
    pub cfg: Config,
    pub store: Mutex<Store>,
    pub pairs: Mutex<BTreeMap<String, PairState>>,
    /// One paper account shared by every pair. Lock order is always `pairs` first, then `account`.
    pub account: Mutex<Sim>,
    /// Risk limits applied to every pair on the next candle. Persisted, so a change survives restarts.
    pub risk: Mutex<RiskConfig>,
    pub http: reqwest::Client,
    pub started: i64,
    /// Latest USD to UGX close and when it was read.
    pub ugx: Mutex<Option<(f64, i64)>>,
    /// Consecutive feed failures per pair, and whether the owner was already told.
    fails: Mutex<HashMap<String, (u32, bool)>>,
    /// When each pair was last polled, success or not, so a failing pair is not retried every tick.
    attempts: Mutex<HashMap<String, i64>>,
    pub update: Mutex<crate::update::UpdateState>,
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn new_driver() -> Driver {
    Driver::new(StrategyConfig::default(), RiskConfig::default())
}

fn new_account() -> Sim {
    Sim::new(SimConfig::default(), START_EQUITY)
}

impl App {
    pub fn new(cfg: Config) -> Result<Self> {
        let store = Store::open(&format!("{}/fx.db", cfg.data_dir))?;
        Self::with_store(cfg, store)
    }

    pub fn with_store(cfg: Config, store: Store) -> Result<Self> {
        let mut pairs = BTreeMap::new();
        for p in &cfg.pairs {
            let saved = store.get(&format!("pair:{p}"))?;
            if let Some(st) = saved.and_then(|j| serde_json::from_str::<PairState>(&j).ok()) {
                pairs.insert(p.clone(), st);
            }
        }
        let account = store
            .get("account")?
            .and_then(|j| serde_json::from_str::<Sim>(&j).ok())
            .unwrap_or_else(new_account);
        let risk = store
            .get("risk")?
            .and_then(|j| serde_json::from_str::<RiskConfig>(&j).ok())
            .unwrap_or_default();
        let ugx = store
            .get("ugx")?
            .and_then(|j| serde_json::from_str::<(f64, i64)>(&j).ok());
        Ok(Self {
            cfg,
            store: Mutex::new(store),
            pairs: Mutex::new(pairs),
            account: Mutex::new(account),
            risk: Mutex::new(risk),
            http: yahoo::client()?,
            started: now(),
            ugx: Mutex::new(ugx),
            fails: Mutex::new(HashMap::new()),
            attempts: Mutex::new(HashMap::new()),
            update: Mutex::new(crate::update::UpdateState::default()),
        })
    }

    pub fn journal(&self, kind: &str, text: &str) {
        if let Ok(s) = self.store.lock() {
            let _ = s.log(now(), kind, text);
        }
    }

    /// Fetches new candles for `pair` and feeds the ones that closed since the last poll.
    pub async fn poll_pair(&self, pair: &str, force: bool) -> Result<usize> {
        let t = now();
        if !force && !self.poll_due(pair, t) {
            return Ok(0);
        }
        if let Ok(mut a) = self.attempts.lock() {
            a.insert(pair.to_string(), t);
        }
        // History is fetched in full once, and again only after a long gap. Everything else asks
        // for the last few days, because mobile data costs the owner money.
        let full = self
            .store
            .lock()
            .ok()
            .and_then(|s| s.latest_ts(pair, self.cfg.tf).ok().flatten())
            .is_none_or(|latest| t - latest > FULL_REFETCH_GAP_SECS);
        let fetched = yahoo::fetch(&self.http, pair, self.cfg.tf, t, full).await;
        let candles = match fetched {
            Ok((c, bytes)) => {
                self.record_bytes(bytes);
                c
            }
            Err(e) => {
                self.set_feed_error(pair, Some(e.to_string()), t);
                return Err(e);
            }
        };
        let history = {
            let mut s = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("store lock poisoned"))?;
            let tf = self.cfg.tf;
            let cutoff = s
                .latest_ts(pair, tf)?
                .map(|l| l - TAIL_REFETCH_BARS * tf.secs());
            let fresh: Vec<Candle> = candles
                .into_iter()
                .filter(|c| cutoff.is_none_or(|k| c.ts >= k))
                .collect();
            s.upsert_candles(pair, tf, &fresh)?;
            s.load(pair, tf, HISTORY_BARS)?
        };
        self.feed(pair, &history, t)
    }

    /// Adds downloaded market-data bytes to today's and this month's totals.
    pub fn record_bytes(&self, n: usize) {
        let (y, m, d) = civil_date(now());
        let Ok(store) = self.store.lock() else { return };
        for key in [
            format!("data:{y:04}-{m:02}"),
            format!("data:{y:04}-{m:02}-{d:02}"),
        ] {
            let total = store
                .get(&key)
                .ok()
                .flatten()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            let _ = store.set(&key, &(total + n as u64).to_string());
        }
    }

    /// Bytes of market data downloaded today and this month.
    pub fn data_usage(&self) -> (u64, u64) {
        let (y, m, d) = civil_date(now());
        let Ok(store) = self.store.lock() else {
            return (0, 0);
        };
        let read = |k: String| {
            store
                .get(&k)
                .ok()
                .flatten()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0)
        };
        (
            read(format!("data:{y:04}-{m:02}-{d:02}")),
            read(format!("data:{y:04}-{m:02}")),
        )
    }

    /// Refreshes the USD to UGX rate when it is missing or older than six hours. A failure keeps
    /// the old rate: the figure is a convenience, never an input to trading.
    pub async fn refresh_ugx(&self) {
        let t = now();
        let fresh = self
            .ugx
            .lock()
            .ok()
            .and_then(|g| *g)
            .is_some_and(|(_, at)| t - at < UGX_MAX_AGE_SECS);
        if fresh {
            return;
        }
        let Ok((rate, bytes)) = yahoo::spot(&self.http, "UGX=X").await else {
            return;
        };
        self.record_bytes(bytes);
        if let Ok(mut g) = self.ugx.lock() {
            *g = Some((rate, t));
        }
        if let Ok(store) = self.store.lock() {
            let _ = store.set(
                "ugx",
                &serde_json::to_string(&(rate, t)).unwrap_or_default(),
            );
        }
    }

    /// Starts the paper account again with `equity`, keeping the learned market structure.
    pub fn reset_account(&self, equity: f64) {
        if let Ok(mut pairs) = self.pairs.lock()
            && let Ok(mut acct) = self.account.lock()
        {
            *acct = Sim::new(SimConfig::default(), equity);
            for st in pairs.values_mut() {
                st.driver.reset_pending();
            }
        }
        self.persist_all();
        self.journal(
            "control",
            &format!("paper account reset to {equity:.2} USD"),
        );
    }

    /// Tells the owner about a feed problem once it has lasted two polls, and again when it ends.
    pub fn note_feed_result(&self, pair: &str, error: Option<&str>) {
        let Ok(mut fails) = self.fails.lock() else {
            return;
        };
        match error {
            Some(e) => {
                let entry = fails.entry(pair.to_string()).or_insert((0, false));
                entry.0 += 1;
                if entry.0 >= FEED_FAILS_BEFORE_ALERT && !entry.1 {
                    entry.1 = true;
                    drop(fails);
                    self.journal("feed", &format!("{pair}: market data problem: {e}"));
                }
            }
            None => {
                if fails.remove(pair).is_some_and(|(_, told)| told) {
                    drop(fails);
                    self.journal("feed", &format!("{pair}: market data is back"));
                }
            }
        }
    }

    fn poll_due(&self, pair: &str, t: i64) -> bool {
        let wait = (self.cfg.tf.secs() / POLL_FRACTION).max(MIN_POLL_SECS);
        self.attempts
            .lock()
            .map(|m| m.get(pair).is_none_or(|a| t - *a >= wait))
            .unwrap_or(true)
    }

    fn set_feed_error(&self, pair: &str, err: Option<String>, t: i64) {
        if let Ok(mut m) = self.pairs.lock()
            && let Some(s) = m.get_mut(pair)
        {
            s.feed_error = err;
            s.last_poll = t;
        }
    }

    /// Rebuilds the market view on this window's timeline, then trades only the candles that
    /// closed since the last poll.
    fn feed(&self, pair: &str, history: &[Candle], t: i64) -> Result<usize> {
        let spec = pair_spec(pair).ok_or_else(|| anyhow::anyhow!("unsupported pair {pair}"))?;
        let Some(last) = history.last() else {
            return Ok(0);
        };
        let by_bar = group_by_known_at(analyze(history, &self.cfg_detect()), history.len());
        let mut map = self
            .pairs
            .lock()
            .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
        let fresh = !map.contains_key(pair);
        let st = map.entry(pair.to_string()).or_insert_with(|| PairState {
            driver: new_driver(),
            last_ts: i64::MIN,
            last_poll: 0,
            feed_error: None,
        });
        st.last_poll = t;
        st.feed_error = None;
        if let Ok(r) = self.risk.lock() {
            st.driver.risk = r.clone();
        }
        let mut acct = self
            .account
            .lock()
            .map_err(|_| anyhow::anyhow!("account lock poisoned"))?;
        let seen = if fresh { last.ts } else { st.last_ts };
        let first_new = st.driver.rebuild_strategy(&spec, history, &by_bar, seen);
        let mut lines = Vec::new();
        if fresh {
            st.last_ts = last.ts;
            lines.push(format!(
                "{pair}: warmed up on {} candles, paper account starts now",
                history.len()
            ));
        } else {
            for (i, c) in history.iter().enumerate().skip(first_new) {
                let out = st.driver.on_bar(&mut acct, &spec, i, c, &by_bar[i]);
                lines.extend(out.into_iter().map(|l| format!("{pair}: {l}")));
                st.last_ts = c.ts;
            }
        }
        let rows = vec![
            (format!("pair:{pair}"), serde_json::to_string(&*st)?),
            ("account".to_string(), serde_json::to_string(&*acct)?),
        ];
        // Locks stay held while writing, in the order pairs, account, store, so a pause set at the
        // same moment cannot be overwritten by this older copy.
        let store = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("store lock poisoned"))?;
        store.set_many(&rows)?;
        for l in &lines {
            store.log(t, "paper", l)?;
        }
        Ok(lines.len())
    }

    fn cfg_detect(&self) -> fx_core::detect::DetectConfig {
        StrategyConfig::default().detect_cfg()
    }

    pub fn set_kill_switch(&self, on: bool) {
        if let Ok(mut acct) = self.account.lock() {
            acct.kill_switch = on;
        }
        self.persist_all();
    }

    /// Writes every pair's state, the account and the risk limits in one transaction. A pause or
    /// risk change must survive a restart that happens before the next candle arrives.
    pub fn persist_all(&self) {
        let (Ok(pairs), Ok(acct), Ok(risk)) =
            (self.pairs.lock(), self.account.lock(), self.risk.lock())
        else {
            return;
        };
        let mut rows: Vec<(String, String)> = pairs
            .iter()
            .filter_map(|(p, st)| Some((format!("pair:{p}"), serde_json::to_string(st).ok()?)))
            .collect();
        if let Ok(j) = serde_json::to_string(&*acct) {
            rows.push(("account".into(), j));
        }
        if let Ok(j) = serde_json::to_string(&*risk) {
            rows.push(("risk".into(), j));
        }
        if let Ok(store) = self.store.lock() {
            let _ = store.set_many(&rows);
        }
    }

    pub fn prune_journal(&self) {
        if let Ok(store) = self.store.lock() {
            let _ = store.prune_journal(JOURNAL_KEEP);
        }
    }
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
pub fn civil_date(t: i64) -> (i64, i64, i64) {
    let z = t.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Polls every configured pair forever. A failed pair is retried on the next tick.
pub async fn scheduler(app: std::sync::Arc<App>) {
    let mut tick: u64 = 0;
    loop {
        tick += 1;
        if tick % PRUNE_EVERY_TICKS == 1 {
            app.prune_journal();
        }
        app.refresh_ugx().await;
        for pair in app.cfg.pairs.clone() {
            match app.poll_pair(&pair, false).await {
                Ok(_) => app.note_feed_result(&pair, None),
                Err(e) => app.note_feed_result(&pair, Some(&e.to_string())),
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(TICK_SECS)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::civil_date;

    #[test]
    fn civil_dates_match_known_days() {
        assert_eq!(civil_date(0), (1970, 1, 1));
        assert_eq!(civil_date(1_704_067_200), (2024, 1, 1));
        assert_eq!(civil_date(1_709_164_800), (2024, 2, 29));
        assert_eq!(civil_date(1_795_523_000), (2026, 11, 24));
    }
}
