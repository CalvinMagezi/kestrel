//! Replays candles through the same `Driver` the live loop uses, then validates the result.

use crate::detect::{Event, analyze};
use crate::risk::RiskConfig;
use crate::sim::{Sim, SimConfig, Trade};
use crate::stats::{
    DrawdownDist, Interval, Rng, Summary, WindowStat, default_bootstrap, monte_carlo_drawdown,
    summarize, walk_forward,
};
use crate::strategy::{Driver, StrategyConfig};
use crate::types::{Candle, pair_spec};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const WALK_FORWARD_WINDOWS: usize = 4;
const MC_RUNS: usize = 1_000;
const VALIDATION_SEED: u64 = 42;
const MIN_BARS: usize = 100;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BacktestConfig {
    pub initial_equity: f64,
    pub strategy: StrategyConfig,
    pub risk: RiskConfig,
    pub sim: SimConfig,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            initial_equity: 10_000.0,
            strategy: StrategyConfig::default(),
            risk: RiskConfig::default(),
            sim: SimConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub pair: String,
    pub bars: usize,
    pub first_ts: i64,
    pub last_ts: i64,
    pub summary: Summary,
    pub bootstrap: Option<Interval>,
    pub drawdown: Option<DrawdownDist>,
    pub walk_forward: Vec<WindowStat>,
    pub baseline: Option<Baseline>,
    pub rejects: BTreeMap<String, usize>,
    pub trades: Vec<Trade>,
}

/// Same stop and target distances as the strategy, but random direction at random bars.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Baseline {
    pub trades: usize,
    pub expectancy_r: f64,
    pub win_rate: f64,
}

pub fn group_by_known_at(events: Vec<Event>, bars: usize) -> Vec<Vec<Event>> {
    let mut out = vec![Vec::new(); bars];
    for ev in events {
        if ev.known_at < bars {
            out[ev.known_at].push(ev);
        }
    }
    out
}

pub fn run(pair: &str, candles: &[Candle], cfg: &BacktestConfig) -> Result<Report, String> {
    let spec = pair_spec(pair).ok_or_else(|| format!("unsupported pair {pair}"))?;
    if candles.len() < MIN_BARS {
        return Err(format!(
            "need at least {MIN_BARS} candles, got {}",
            candles.len()
        ));
    }
    let by_bar = group_by_known_at(analyze(candles, &cfg.strategy.detect_cfg()), candles.len());
    let mut driver = Driver::new(cfg.strategy.clone(), cfg.risk.clone());
    let mut sim = Sim::new(cfg.sim.clone(), cfg.initial_equity);
    for (t, c) in candles.iter().enumerate() {
        driver.on_bar(&mut sim, &spec, t, c, &by_bar[t]);
    }
    let last = candles[candles.len() - 1];
    let open_ids: Vec<u64> = sim.positions.iter().map(|p| p.id).collect();
    for id in open_ids {
        sim.close_manual(&spec, id, last.close, last.ts);
    }
    let trades = sim.trades.clone();
    let rs: Vec<f64> = trades.iter().map(|t| t.r).collect();
    Ok(Report {
        pair: spec.name.to_string(),
        bars: candles.len(),
        first_ts: candles[0].ts,
        last_ts: last.ts,
        summary: summarize(&trades, cfg.initial_equity),
        bootstrap: default_bootstrap(&rs, VALIDATION_SEED),
        drawdown: monte_carlo_drawdown(&rs, cfg.risk.risk_per_trade_pct, MC_RUNS, VALIDATION_SEED),
        walk_forward: walk_forward(&trades, WALK_FORWARD_WINDOWS),
        baseline: random_baseline(pair, candles, cfg, &trades),
        rejects: driver.rejects.clone(),
        trades,
    })
}

/// Random entries that use the strategy's own stop-distance and reward ratio, under the same
/// costs. If the strategy cannot beat this, its structure logic adds nothing.
fn random_baseline(
    pair: &str,
    candles: &[Candle],
    cfg: &BacktestConfig,
    real: &[Trade],
) -> Option<Baseline> {
    let spec = pair_spec(pair)?;
    if real.len() < 5 {
        return None;
    }
    let mut rng = Rng::new(VALIDATION_SEED);
    let mut sim = Sim::new(cfg.sim.clone(), cfg.initial_equity);
    let every = (candles.len() / real.len()).max(1);
    let mut next_entry = rng.below(every);
    let stop_dist = median_stop_distance(real, spec.pip);
    for (t, c) in candles.iter().enumerate() {
        sim.on_candle(&spec, c);
        if t == next_entry && sim.positions.is_empty() {
            let dir = if rng.below(2) == 0 {
                crate::types::Dir::Bull
            } else {
                crate::types::Dir::Bear
            };
            let s = dir.sign();
            let order = crate::sim::OpenOrder {
                dir,
                units: 10_000.0,
                mid: c.close,
                stop: c.close - s * stop_dist,
                target: c.close + s * stop_dist * cfg.strategy.rr,
                ts: c.ts,
                note: "random",
            };
            sim.open(&spec, order);
        }
        if t >= next_entry {
            next_entry = t + 1 + rng.below(every * 2);
        }
    }
    let n = sim.trades.len();
    if n == 0 {
        return None;
    }
    let wins = sim.trades.iter().filter(|t| t.r > 0.0).count();
    Some(Baseline {
        trades: n,
        expectancy_r: sim.trades.iter().map(|t| t.r).sum::<f64>() / n as f64,
        win_rate: wins as f64 / n as f64,
    })
}

fn median_stop_distance(trades: &[Trade], pip: f64) -> f64 {
    let mut d: Vec<f64> = trades.iter().map(|t| t.stop_pips * pip).collect();
    d.sort_by(f64::total_cmp);
    let m = d[d.len() / 2];
    if m > 0.0 { m } else { 20.0 * pip }
}

pub fn describe(r: &Report) -> String {
    let s = &r.summary;
    let mut out = format!(
        "{} {} bars: {} trades, win {:.0}%, expectancy {:+.2} R, profit factor {:.2}, max drawdown {:.1}%",
        r.pair,
        r.bars,
        s.trades,
        s.win_rate * 100.0,
        s.expectancy_r,
        s.profit_factor,
        s.max_drawdown_pct
    );
    if let Some(ci) = &r.bootstrap {
        out.push_str(&format!(
            "\n95% interval of expectancy: {:+.2} to {:+.2} R (chance above zero {:.0}%)",
            ci.low,
            ci.high,
            ci.prob_positive * 100.0
        ));
    }
    if let Some(b) = &r.baseline {
        out.push_str(&format!(
            "\nRandom entries, same stops and costs: {:+.2} R over {} trades",
            b.expectancy_r, b.trades
        ));
    }
    if !r.rejects.is_empty() {
        let parts: Vec<String> = r.rejects.iter().map(|(k, n)| format!("{k} x{n}")).collect();
        out.push_str(&format!("\nSkipped by risk rules: {}", parts.join("; ")));
    }
    if !r.walk_forward.is_empty() {
        let parts: Vec<String> = r
            .walk_forward
            .iter()
            .map(|w| format!("{:+.2}", w.expectancy_r))
            .collect();
        out.push_str(&format!("\nExpectancy by period: {}", parts.join(", ")));
    }
    out
}
