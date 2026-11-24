//! The chat. Commands are deterministic and need no model; free text goes to a local Ollama
//! model only if one is configured.

use crate::app::{App, HISTORY_BARS, now};
use fx_core::backtest::{BacktestConfig, Report, describe, run};
use fx_core::pair_spec;
use hq_tools_web::web::{SearchOptions, web_fetch_direct, web_search};
use std::fmt::Write;

const HELP: &str = "Commands:\n\
status: equity, open positions, feed health\n\
setups [pair]: what the detectors see now\n\
positions: open paper trades\n\
journal [n]: recent events\n\
backtest [pair]: replay stored candles through the strategy\n\
refresh: fetch new candles now\n\
verify: check the journal has not been edited\n\
risk [percent]: show or set risk per trade (0.1 to 2)\n\
pause / resume: stop or allow new entries\n\
news [pair]: web search for the pair\n\
search <query>: web search\n\
read <url>: fetch a page as text\n\
Anything else goes to the local model if one is set up.";

pub const MAX_RISK_PCT: f64 = 2.0;
pub const MIN_RISK_PCT: f64 = 0.1;
const SEARCH_RESULTS: usize = 5;
const FETCH_CHARS: usize = 2_500;
const BACKTEST_NOTE: &str =
    "Past data, mid prices with modelled costs. This is a measurement, not a forecast.";

/// One earlier message, sent by the page so the model can follow the conversation.
#[derive(serde::Deserialize, Clone)]
pub struct Turn {
    pub who: String,
    pub text: String,
}

const MAX_TURNS: usize = 8;
const MAX_TURN_CHARS: usize = 1_500;

/// Builds the alternating user and assistant messages a model API expects: it starts with a user
/// message, consecutive messages from one side are joined, and the new question comes last.
pub fn conversation(turns: &[Turn], question: &str) -> Vec<serde_json::Value> {
    let mut msgs: Vec<(&str, String)> = Vec::new();
    let start = turns.len().saturating_sub(MAX_TURNS);
    for t in &turns[start..] {
        let role = if t.who == "me" { "user" } else { "assistant" };
        let text: String = t.text.chars().take(MAX_TURN_CHARS).collect();
        if text.trim().is_empty() {
            continue;
        }
        match msgs.last_mut() {
            Some((r, body)) if *r == role => {
                body.push('\n');
                body.push_str(&text);
            }
            _ => msgs.push((role, text)),
        }
    }
    while msgs.first().is_some_and(|(r, _)| *r == "assistant") {
        msgs.remove(0);
    }
    match msgs.last_mut() {
        Some(("user", body)) => {
            body.push('\n');
            body.push_str(question);
        }
        _ => msgs.push(("user", question.to_string())),
    }
    msgs.into_iter()
        .map(|(r, b)| serde_json::json!({"role": r, "content": b}))
        .collect()
}

/// A chat answer: plain text, plus an optional structured card the page can draw as a table.
pub struct Reply {
    pub text: String,
    pub card: Option<serde_json::Value>,
}

pub async fn handle(app: &App, text: &str, turns: &[Turn]) -> Reply {
    let text = text.trim();
    let (cmd, rest) = text
        .split_once(char::is_whitespace)
        .map_or((text, ""), |(c, r)| (c, r.trim()));
    if cmd.eq_ignore_ascii_case("backtest") && is_command("backtest", rest) {
        return backtest(app, rest).await;
    }
    Reply {
        text: dispatch(app, text, turns).await,
        card: None,
    }
}

async fn dispatch(app: &App, text: &str, turns: &[Turn]) -> String {
    let text = text.trim();
    let (cmd, rest) = text
        .split_once(char::is_whitespace)
        .map_or((text, ""), |(c, r)| (c, r.trim()));
    let cmd = cmd.to_ascii_lowercase();
    if !is_command(&cmd, rest) {
        return ask_model(app, text, turns).await;
    }
    match cmd.as_str() {
        "" => "Say something, or type help.".into(),
        "help" | "?" => HELP.into(),
        "status" => status(app),
        "positions" => positions(app),
        "setups" | "explain" => setups(app, rest),
        "journal" => journal(app, rest),
        "backtest" => backtest(app, rest).await.text,
        "refresh" => refresh(app).await,
        "verify" => verify(app),
        "risk" => risk(app, rest),
        "pause" => toggle(app, true),
        "resume" => toggle(app, false),
        "news" => news(app, rest).await,
        "search" => search(rest).await,
        "read" | "fetch" => read(rest).await,
        _ => ask_model(app, text, turns).await,
    }
}

/// A word only counts as a command when its arguments fit. Otherwise a sentence that happens to
/// start with "explain" or "status" would be swallowed instead of reaching the model.
fn is_command(cmd: &str, rest: &str) -> bool {
    let one_pair_or_none = rest.is_empty() || (!rest.contains(' ') && pair_spec(rest).is_some());
    match cmd {
        "" | "help" | "?" | "status" | "positions" | "refresh" | "pause" | "resume" | "verify" => {
            rest.is_empty()
        }
        "setups" | "explain" | "backtest" => one_pair_or_none,
        "journal" => rest.is_empty() || rest.parse::<usize>().is_ok(),
        "risk" => rest.is_empty() || rest.trim_end_matches('%').parse::<f64>().is_ok(),
        "news" => one_pair_or_none,
        "search" | "read" | "fetch" => !rest.is_empty(),
        _ => false,
    }
}

fn verify(app: &App) -> String {
    let status = app.store.lock().expect("store lock").verify_chain();
    let Ok(v) = status else {
        return "Could not read the journal.".into();
    };
    let head: String = v
        .head
        .clone()
        .unwrap_or_default()
        .chars()
        .take(12)
        .collect();
    match (v.entries, v.intact, v.broken_at) {
        (0, _, _) => "The journal has no chained entries yet.".into(),
        (n, true, _) => {
            let (y, m, d) = crate::app::civil_date(v.since.unwrap_or(0));
            format!(
                "Journal intact: {n} entries since {y:04}-{m:02}-{d:02} UTC. Latest seal {head}. Note it somewhere you trust, and a later check can show nothing before it changed."
            )
        }
        (_, false, at) => format!(
            "Journal BROKEN at entry {}. Entries before it still match; nothing after it can be trusted.",
            at.unwrap_or(0)
        ),
    }
}

fn pair_or_first(app: &App, arg: &str) -> String {
    let wanted = arg.split_whitespace().next().unwrap_or("");
    pair_spec(wanted)
        .map(|s| s.name.to_string())
        .unwrap_or_else(|| app.cfg.pairs.first().cloned().unwrap_or_default())
}

pub fn status(app: &App) -> String {
    let m = app.pairs.lock().expect("state lock");
    let acct = app.account.lock().expect("account lock");
    let mut out = format!(
        "Paper account only, no broker. Up {} min.\n",
        (now() - app.started) / 60
    );
    if m.is_empty() {
        out.push_str("No candles processed yet. Try refresh.");
        return out;
    }
    let _ = write!(
        out,
        "Equity {:.2} USD, {} open, {} closed",
        acct.equity,
        acct.positions.len(),
        acct.trades.len()
    );
    if acct.kill_switch {
        out.push_str(", PAUSED");
    }
    out.push('\n');
    for (pair, st) in m.iter() {
        let age = if st.last_ts > 0 {
            ((now() - st.last_ts - app.cfg.tf.secs()).max(0)) / 60
        } else {
            -1
        };
        let _ = write!(out, "{pair}: last closed candle {age} min ago");
        if let Some(e) = &st.feed_error {
            let _ = write!(out, "\n  feed problem: {e}");
        }
        out.push('\n');
    }
    out.trim_end().into()
}

fn positions(app: &App) -> String {
    let acct = app.account.lock().expect("account lock");
    let mut out = String::new();
    for p in &acct.positions {
        let _ = writeln!(
            out,
            "{} {:?} {} units at {:.5}, stop {:.5}, target {:.5} ({})",
            p.pair, p.dir, p.units, p.entry, p.stop, p.target, p.note
        );
    }
    if out.is_empty() {
        "No open positions.".into()
    } else {
        out.trim_end().into()
    }
}

fn setups(app: &App, arg: &str) -> String {
    let pair = pair_or_first(app, arg);
    let m = app.pairs.lock().expect("state lock");
    match m.get(&pair) {
        Some(st) => format!("{pair}: {}", st.driver.strategy.describe()),
        None => format!("No data for {pair} yet. Try refresh."),
    }
}

fn journal(app: &App, arg: &str) -> String {
    let n = arg.parse::<usize>().unwrap_or(10).clamp(1, 50);
    let rows = app
        .store
        .lock()
        .expect("store lock")
        .recent_journal(n)
        .unwrap_or_default();
    if rows.is_empty() {
        return "Journal is empty.".into();
    }
    rows.into_iter()
        .map(|(ts, kind, text)| format!("[{}m ago] {kind}: {text}", (now() - ts) / 60))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn backtest(app: &App, arg: &str) -> Reply {
    let plain = |text: String| Reply { text, card: None };
    let pair = pair_or_first(app, arg);
    let candles = match app
        .store
        .lock()
        .expect("store lock")
        .load(&pair, app.cfg.tf, HISTORY_BARS)
    {
        Ok(c) if !c.is_empty() => c,
        _ => return plain(format!("No stored candles for {pair}. Try refresh first.")),
    };
    let result =
        tokio::task::spawn_blocking(move || run(&pair, &candles, &BacktestConfig::default())).await;
    match result {
        Ok(Ok(report)) => Reply {
            text: format!("{}\n{BACKTEST_NOTE}", describe(&report)),
            card: Some(backtest_card(&report, &format!("{:?}", app.cfg.tf))),
        },
        Ok(Err(e)) => plain(format!("Backtest could not run: {e}")),
        Err(_) => plain("Backtest crashed.".into()),
    }
}

/// Rows are `[label, value, trend]`; trend is "up", "down" or "flat" and only the expectancy row
/// carries one, so the page can pair the figure with an arrow instead of a color.
pub fn backtest_card(r: &Report, tf: &str) -> serde_json::Value {
    let s = &r.summary;
    let trend = if s.expectancy_r > 0.005 {
        "up"
    } else if s.expectancy_r < -0.005 {
        "down"
    } else {
        "flat"
    };
    let pf = if s.profit_factor.is_finite() {
        format!("{:.2}", s.profit_factor)
    } else {
        "n/a".to_string()
    };
    let mut rows = vec![
        serde_json::json!(["Trades", s.trades.to_string(), ""]),
        serde_json::json!(["Win rate", format!("{:.0}%", s.win_rate * 100.0), ""]),
        serde_json::json!(["Expectancy", format!("{:+.2} R", s.expectancy_r), trend]),
        serde_json::json!(["Profit factor", pf, ""]),
        serde_json::json!(["Max drawdown", format!("{:.1}%", s.max_drawdown_pct), ""]),
    ];
    if let Some(ci) = &r.bootstrap {
        rows.push(serde_json::json!([
            "95% interval",
            format!("{:+.2} to {:+.2} R", ci.low, ci.high),
            ""
        ]));
    }
    if let Some(b) = &r.baseline {
        rows.push(serde_json::json!([
            "Random entries",
            format!("{:+.2} R", b.expectancy_r),
            ""
        ]));
    }
    serde_json::json!({
        "title": format!("{}, last {} {tf} bars", r.pair, r.bars),
        "rows": rows,
        "note": BACKTEST_NOTE,
    })
}

async fn refresh(app: &App) -> String {
    let mut out = Vec::new();
    for pair in app.cfg.pairs.clone() {
        match app.poll_pair(&pair, true).await {
            Ok(n) => out.push(format!("{pair}: ok, {n} new event(s)")),
            Err(e) => out.push(format!("{pair}: failed, {e}")),
        }
    }
    out.join("\n")
}

fn risk(app: &App, arg: &str) -> String {
    if arg.is_empty() {
        let pct = app.risk.lock().expect("risk lock").risk_per_trade_pct;
        return format!("Risk per trade is {pct}% of equity.");
    }
    let Some(pct) = arg
        .trim_end_matches('%')
        .parse::<f64>()
        .ok()
        .filter(|p| (MIN_RISK_PCT..=MAX_RISK_PCT).contains(p))
    else {
        return format!("Give a number from {MIN_RISK_PCT} to {MAX_RISK_PCT}.");
    };
    app.risk.lock().expect("risk lock").risk_per_trade_pct = pct;
    app.persist_all();
    format!("Risk per trade set to {pct}%.")
}

fn toggle(app: &App, pause: bool) -> String {
    app.set_kill_switch(pause);
    app.journal(
        "control",
        if pause {
            "paused by user"
        } else {
            "resumed by user"
        },
    );
    if pause {
        "Paused: no new entries. Open trades still run to their stop or target.".into()
    } else {
        "Resumed.".into()
    }
}

async fn news(app: &App, arg: &str) -> String {
    let pair = pair_or_first(app, arg);
    search(&format!("{pair} forex outlook")).await
}

async fn search(query: &str) -> String {
    if query.len() < 2 {
        return "Give a search query.".into();
    }
    let opts = SearchOptions {
        max_results: SEARCH_RESULTS,
        ..SearchOptions::default()
    };
    match web_search(query, &opts, None, None, true).await {
        Ok(r) if r.results.is_empty() => "No results.".into(),
        Ok(r) => r
            .results
            .iter()
            .map(|x| format!("{}\n{}\n{}", x.title, x.url, x.snippet))
            .collect::<Vec<_>>()
            .join("\n\n"),
        Err(e) => format!("Search failed: {e}"),
    }
}

async fn read(url: &str) -> String {
    match web_fetch_direct(url, FETCH_CHARS).await {
        Ok(p) => format!("{}\n\n{}", p.final_url, p.content),
        Err(e) => format!("Could not read the page: {e}"),
    }
}

pub const CLAUDE_MODEL: &str = "claude-haiku-5-5";
const CLAUDE_URL: &str = "https://api.anthropic.com/v1/messages";
const CLAUDE_MAX_TOKENS: u32 = 700;

fn model_context(app: &App) -> String {
    format!(
        "You are the assistant inside a forex paper-trading research harness running on the owner's phone. \
It is a paper account with no real money. Explain what the harness shows, answer questions about its \
state and about smart money concepts, and be plain about uncertainty. Never give financial advice and \
never claim the strategy has an edge unless the data shown says so. Answer in at most four short sentences unless the user asks for detail. Write plain text only: no markdown, no bold, no headings, no bullet lists. Current state:\n{}\n{}",
        status(app),
        setups(app, "")
    )
}

async fn ask_claude(app: &App, key: &str, text: &str, turns: &[Turn]) -> String {
    let body = serde_json::json!({
        "model": CLAUDE_MODEL,
        "max_tokens": CLAUDE_MAX_TOKENS,
        "system": model_context(app),
        "messages": conversation(turns, text),
    });
    let sent = app
        .http
        .post(CLAUDE_URL)
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await;
    let resp = match sent {
        Ok(r) => r,
        Err(e) => return format!("Could not reach the model: {}", e.without_url()),
    };
    let status = resp.status();
    let Ok(v) = resp.json::<serde_json::Value>().await else {
        return format!("Model reply unreadable (HTTP {status}).");
    };
    if !status.is_success() {
        let why = v["error"]["message"].as_str().unwrap_or("no detail");
        return format!("Model request failed (HTTP {status}): {why}");
    }
    reply_text(&v)
}

/// The visible answer is every `text` block joined. Newer models may put a `thinking` block first,
/// so reading only the first block can find nothing.
pub fn reply_text(v: &serde_json::Value) -> String {
    let joined: String = v["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let joined = joined.trim();
    if joined.is_empty() {
        "The model returned nothing.".to_string()
    } else {
        joined.to_string()
    }
}

async fn ask_model(app: &App, text: &str, turns: &[Turn]) -> String {
    if let Some(key) = &app.cfg.anthropic_key {
        return ask_claude(app, key, text, turns).await;
    }
    let Some(url) = &app.cfg.ollama_url else {
        return "I only understand commands without a local model. Type help. To chat freely, put a key in anthropic.key in the data folder, or run Ollama and start fxh with --ollama <url>.".into();
    };
    let context = format!(
        "You are the assistant inside a forex paper-trading research harness. Never give financial advice. Current state:\n{}\n{}",
        status(app),
        setups(app, "")
    );
    let mut messages = vec![serde_json::json!({"role": "system", "content": context})];
    messages.extend(conversation(turns, text));
    let body = serde_json::json!({
        "model": app.cfg.ollama_model,
        "stream": false,
        "messages": messages,
    });
    let sent = app
        .http
        .post(format!("{url}/api/chat"))
        .json(&body)
        .send()
        .await;
    match sent {
        Ok(r) => match r.json::<serde_json::Value>().await {
            Ok(v) => v["message"]["content"]
                .as_str()
                .unwrap_or("The model returned nothing.")
                .trim()
                .to_string(),
            Err(e) => format!("Model reply unreadable: {e}"),
        },
        Err(e) => format!("Could not reach the local model at {url}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{Turn, conversation, is_command, reply_text};
    use serde_json::json;

    fn t(who: &str, text: &str) -> Turn {
        Turn {
            who: who.into(),
            text: text.into(),
        }
    }

    #[test]
    fn conversation_alternates_and_ends_with_the_question() {
        let turns = [
            t("bot", "hello"),
            t("me", "backtest"),
            t("bot", "result"),
            t("bot", "note"),
        ];
        let m = conversation(&turns, "what does that mean?");
        let roles: Vec<&str> = m.iter().map(|x| x["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "assistant", "user"]);
        assert_eq!(m[1]["content"], "result\nnote");
        assert_eq!(m[2]["content"], "what does that mean?");
    }

    #[test]
    fn a_question_after_a_user_turn_is_merged_and_empty_history_is_fine() {
        let m = conversation(&[t("me", "first")], "second");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0]["content"], "first\nsecond");
        assert_eq!(conversation(&[], "hi").len(), 1);
    }

    #[test]
    fn reply_text_skips_thinking_blocks() {
        let v = json!({"content": [{"type": "thinking", "thinking": ""}, {"type": "text", "text": " Answer. "}]});
        assert_eq!(reply_text(&v), "Answer.");
        assert_eq!(
            reply_text(&json!({"content": []})),
            "The model returned nothing."
        );
    }

    #[test]
    fn sentences_starting_with_a_command_word_go_to_the_model() {
        assert!(!is_command("explain", "in two sentences what a CHoCH is"));
        assert!(!is_command("status", "of my paper account please"));
        assert!(!is_command("risk", "is too high today"));
    }

    #[test]
    fn real_commands_still_match() {
        assert!(is_command("status", ""));
        assert!(is_command("explain", "EURUSD"));
        assert!(is_command("backtest", "gbpusd"));
        assert!(is_command("journal", "20"));
        assert!(is_command("risk", "0.5%"));
        assert!(is_command("search", "eurusd outlook this week"));
        assert!(!is_command("search", ""));
    }
}
