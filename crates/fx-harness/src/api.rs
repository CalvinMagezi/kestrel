//! Data for the overview and control screens, built as plain JSON values so they can be tested
//! without a server.

use crate::app::{App, civil_date, now};
use crate::chat::{CLAUDE_MODEL, MAX_RISK_PCT, MIN_RISK_PCT};
use fx_core::Dir;
use serde_json::{Value, json};

const RECENT_TRADES: usize = 5;
const MAX_ZONES: usize = 12;
const STALE_CANDLES: i64 = 3;
const SECS_PER_DAY: i64 = 86_400;
const SECS_PER_HOUR: i64 = 3_600;
const FRIDAY: i64 = 4;
const SATURDAY: i64 = 5;
const SUNDAY: i64 = 6;
const FX_CLOSE_HOUR: i64 = 22;
const HEAD_CHARS: usize = 12;
const BYTES_PER_KB: u64 = 1_024;
const MIN_EQUITY: f64 = 10.0;
const MAX_EQUITY: f64 = 1_000_000.0;
const ALERT_KINDS: &[&str] = &["paper", "feed", "update"];
const MAX_EVENTS: usize = 20;
const RESET_WORD: &str = "RESET";

/// FX trades from Sunday 22:00 UTC to Friday 22:00 UTC.
pub fn market_closed(t: i64) -> bool {
    let dow = (t.div_euclid(SECS_PER_DAY) + 3).rem_euclid(7);
    let hour = t.rem_euclid(SECS_PER_DAY) / SECS_PER_HOUR;
    (dow == FRIDAY && hour >= FX_CLOSE_HOUR)
        || dow == SATURDAY
        || (dow == SUNDAY && hour < FX_CLOSE_HOUR)
}

fn dir_word(d: Dir) -> &'static str {
    match d {
        Dir::Bull => "bull",
        Dir::Bear => "bear",
    }
}

pub fn overview(app: &App) -> Value {
    let pairs = app.pairs.lock().expect("state lock");
    let acct = app.account.lock().expect("account lock");
    let t = now();
    let stale_after = app.cfg.tf.secs() * STALE_CANDLES;
    let mut rows = Vec::new();
    let mut zones = Vec::new();
    for pair in &app.cfg.pairs {
        let Some(st) = pairs.get(pair) else {
            rows.push(json!({"pair": pair, "trend": "none", "zones": 0, "age_min": null, "state": "warming"}));
            continue;
        };
        // Minutes since the last candle closed, not since it opened.
        let age = (t - st.last_ts - app.cfg.tf.secs()).max(0);
        let state = if st.feed_error.is_some() {
            "problem"
        } else if market_closed(t) {
            "closed"
        } else if age > stale_after {
            "stale"
        } else {
            "live"
        };
        let trend = st.driver.strategy.trend().map_or("none", dir_word);
        let live = st.driver.strategy.zones();
        rows.push(json!({
            "pair": pair, "trend": trend, "zones": live.len(),
            "age_min": age / 60, "state": state, "error": st.feed_error,
        }));
        zones.extend(live.into_iter().map(|z| {
            json!({"pair": pair, "dir": dir_word(z.dir), "label": z.label, "bottom": z.bottom, "top": z.top})
        }));
    }
    zones.truncate(MAX_ZONES);
    let positions: Vec<Value> = acct.positions.iter().map(|p| {
        json!({"pair": p.pair, "dir": dir_word(p.dir), "units": p.units, "entry": p.entry, "stop": p.stop, "target": p.target, "note": p.note})
    }).collect();
    let recent: Vec<Value> = acct
        .trades
        .iter()
        .rev()
        .take(RECENT_TRADES)
        .map(|tr| json!({"pair": tr.pair, "dir": dir_word(tr.dir), "r": tr.r, "pnl": tr.pnl_usd}))
        .collect();
    let max_open = app.risk.lock().expect("risk lock").max_open_positions;
    json!({
        "equity": acct.equity,
        "change": acct.equity - acct.start_equity,
        "start_equity": acct.start_equity,
        "ugx": ugx_json(app),
        "paused": acct.kill_switch,
        "open": acct.positions.len(),
        "max_open": max_open,
        "closed": acct.trades.len(),
        "pairs": rows, "zones": zones, "positions": positions, "recent": recent,
        "market_closed": market_closed(t),
    })
}

fn ugx_json(app: &App) -> Value {
    match app.ugx.lock().ok().and_then(|g| *g) {
        Some((rate, at)) => json!({"rate": rate, "age_hours": (now() - at) / SECS_PER_HOUR}),
        None => Value::Null,
    }
}

pub fn control(app: &App) -> Value {
    let (paused, start_equity) = {
        let acct = app.account.lock().expect("account lock");
        (acct.kill_switch, acct.start_equity)
    };
    let risk_pct = app.risk.lock().expect("risk lock").risk_per_trade_pct;
    let assistant = if app.cfg.anthropic_key.is_some() {
        json!({"kind": "claude", "model": "Haiku 5.5", "id": CLAUDE_MODEL, "ready": true})
    } else if app.cfg.ollama_url.is_some() {
        json!({"kind": "ollama", "model": app.cfg.ollama_model, "ready": true})
    } else {
        json!({"kind": "none", "model": null, "ready": false})
    };
    let (today, month) = app.data_usage();
    let chain = app.store.lock().ok().and_then(|s| s.verify_chain().ok());
    let journal = chain.map(|c| {
        json!({
            "entries": c.entries, "intact": c.intact, "since": c.since, "broken_at": c.broken_at,
            "head": c.head.map(|h| h.chars().take(HEAD_CHARS).collect::<String>()),
        })
    });
    json!({
        "paused": paused,
        "risk_pct": risk_pct, "risk_min": MIN_RISK_PCT, "risk_max": MAX_RISK_PCT,
        "assistant": assistant,
        "timeframe": format!("{:?}", app.cfg.tf),
        "pairs": app.cfg.pairs,
        "account": {"start_equity": start_equity, "min": MIN_EQUITY, "max": MAX_EQUITY},
        "data": {"today_kb": today / BYTES_PER_KB, "month_kb": month / BYTES_PER_KB},
        "journal": journal,
        "ugx": ugx_json(app),
        "app_version": app.cfg.app_version,
    })
}

/// Journal rows after `since` that are worth a notification, oldest first. A negative `since`
/// only reports the newest id, so a new listener starts from now instead of replaying history.
pub fn events(app: &App, since: i64) -> Value {
    let store = app.store.lock().expect("store lock");
    let last = store.last_id().unwrap_or(0);
    if since < 0 {
        return json!({"last_id": last, "events": []});
    }
    let rows = store
        .events_since(since, ALERT_KINDS, MAX_EVENTS)
        .unwrap_or_default();
    json!({"last_id": last, "events": rows})
}

/// The journal as one JSON object per line, or the closed trades as CSV.
pub fn export(app: &App, kind: &str) -> Result<(String, &'static str), String> {
    match kind {
        "journal" => {
            let body = app
                .store
                .lock()
                .expect("store lock")
                .export_journal()
                .map_err(|e| e.to_string())?;
            Ok((body, "application/x-ndjson"))
        }
        "trades" => {
            let acct = app.account.lock().expect("account lock");
            let mut out = String::from(
                "id,pair,direction,units,opened_utc,closed_utc,entry,exit,pips,pnl_usd,r,reason\n",
            );
            for t in &acct.trades {
                out.push_str(&format!(
                    "{},{},{:?},{},{},{},{:.5},{:.5},{:.1},{:.2},{:.2},{:?}\n",
                    t.id,
                    t.pair,
                    t.dir,
                    t.units,
                    iso(t.entry_ts),
                    iso(t.exit_ts),
                    t.entry,
                    t.exit,
                    t.pips,
                    t.pnl_usd,
                    t.r,
                    t.reason
                ));
            }
            Ok((out, "text/csv"))
        }
        _ => Err("kind must be journal or trades".into()),
    }
}

fn iso(t: i64) -> String {
    let (y, m, d) = civil_date(t);
    let s = t.rem_euclid(SECS_PER_DAY);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        s / SECS_PER_HOUR,
        s % SECS_PER_HOUR / 60,
        s % 60
    )
}

/// Applies `{"paused": bool}` and/or `{"risk_pct": number}` and returns the new control state.
pub fn set_control(app: &App, body: &Value) -> Result<Value, String> {
    if let Some(p) = body.get("paused") {
        let on = p.as_bool().ok_or("paused must be true or false")?;
        app.set_kill_switch(on);
        app.journal(
            "control",
            if on {
                "paused from the app"
            } else {
                "resumed from the app"
            },
        );
    }
    if let Some(r) = body.get("reset") {
        let equity = r
            .get("equity")
            .and_then(Value::as_f64)
            .filter(|e| (MIN_EQUITY..=MAX_EQUITY).contains(e))
            .ok_or_else(|| format!("equity must be from {MIN_EQUITY} to {MAX_EQUITY}"))?;
        if r.get("confirm").and_then(Value::as_str) != Some(RESET_WORD) {
            return Err(format!("type {RESET_WORD} to confirm"));
        }
        app.reset_account(equity);
    }
    if let Some(v) = body.get("risk_pct") {
        let pct = v
            .as_f64()
            .filter(|p| (MIN_RISK_PCT..=MAX_RISK_PCT).contains(p))
            .ok_or_else(|| format!("risk_pct must be from {MIN_RISK_PCT} to {MAX_RISK_PCT}"))?;
        app.risk.lock().expect("risk lock").risk_per_trade_pct = pct;
        app.persist_all();
    }
    Ok(control(app))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Config;
    use fx_core::Timeframe;
    use fx_feed::Store;

    fn test_app() -> App {
        let cfg = Config {
            data_dir: String::new(),
            port: 0,
            tf: Timeframe::H1,
            pairs: vec!["EURUSD".into()],
            token: "a".repeat(32),
            anthropic_key: None,
            ollama_url: None,
            ollama_model: String::new(),
            update_repo: "o/r".into(),
            app_version: "0.1.0".into(),
        };
        App::with_store(cfg, Store::memory().unwrap()).unwrap()
    }

    #[test]
    fn control_reports_account_data_and_journal() {
        let app = test_app();
        app.journal("paper", "setup ready");
        let c = control(&app);
        assert_eq!(c["account"]["start_equity"], 10_000.0);
        assert_eq!(c["journal"]["entries"], 1);
        assert_eq!(c["journal"]["intact"], true);
        assert_eq!(c["data"]["today_kb"], 0);
        assert_eq!(c["assistant"]["kind"], "none");
    }

    #[test]
    fn reset_needs_the_word_and_a_sane_amount() {
        let app = test_app();
        let bad_word = json!({"reset": {"equity": 100.0, "confirm": "yes"}});
        assert!(set_control(&app, &bad_word).unwrap_err().contains("RESET"));
        let bad_amount = json!({"reset": {"equity": 5.0, "confirm": "RESET"}});
        assert!(set_control(&app, &bad_amount).is_err());
        let ok = json!({"reset": {"equity": 250.0, "confirm": "RESET"}});
        let c = set_control(&app, &ok).unwrap();
        assert_eq!(c["account"]["start_equity"], 250.0);
        assert_eq!(overview(&app)["equity"], 250.0);
    }

    #[test]
    fn risk_outside_the_range_is_refused() {
        let app = test_app();
        assert!(set_control(&app, &json!({"risk_pct": 9.0})).is_err());
        assert_eq!(
            set_control(&app, &json!({"risk_pct": 0.8})).unwrap()["risk_pct"],
            0.8
        );
    }

    #[test]
    fn events_start_from_now_and_then_deliver_new_rows() {
        let app = test_app();
        app.journal("paper", "old");
        let base = events(&app, -1);
        let last = base["last_id"].as_i64().unwrap();
        assert!(base["events"].as_array().unwrap().is_empty());
        app.journal("paper", "new setup");
        app.journal("control", "paused");
        let e = events(&app, last);
        assert_eq!(e["events"].as_array().unwrap().len(), 1);
        assert_eq!(e["events"][0]["text"], "new setup");
    }

    #[test]
    fn exports_have_a_header_and_reject_unknown_kinds() {
        let app = test_app();
        let (csv, mime) = export(&app, "trades").unwrap();
        assert!(csv.starts_with("id,pair,direction") && mime == "text/csv");
        assert!(export(&app, "journal").is_ok());
        assert!(export(&app, "secrets").is_err());
        assert_eq!(iso(1_704_067_200), "2024-01-01T00:00:00Z");
    }

    use super::market_closed;

    // 2024-01-05 is a Friday, 2024-01-07 a Sunday, 2024-01-08 a Monday.
    const FRI_2100: i64 = 1_704_488_400;
    const FRI_2200: i64 = FRI_2100 + 3_600;
    const SAT_NOON: i64 = 1_704_628_800;
    const SUN_2100: i64 = 1_704_661_200;
    const SUN_2200: i64 = SUN_2100 + 3_600;
    const MON_NOON: i64 = 1_704_715_200;

    #[test]
    fn the_weekend_gap_is_closed_and_weekdays_are_open() {
        assert!(!market_closed(FRI_2100));
        assert!(market_closed(FRI_2200));
        assert!(market_closed(SAT_NOON));
        assert!(market_closed(SUN_2100));
        assert!(!market_closed(SUN_2200));
        assert!(!market_closed(MON_NOON));
    }
}
