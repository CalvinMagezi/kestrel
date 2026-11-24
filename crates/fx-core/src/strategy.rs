//! The v1 SMC entry model and the `Driver` that runs it. The same `Driver::on_bar` serves the
//! backtester and the live paper loop, so there is one code path for both.
//!
//! Model: a structure break sets the trend. After it, a fair value gap or order block in the trend
//! direction becomes a setup. When price first trades back into the zone and that candle does not
//! close through it, enter at the next open, stop beyond the far edge of the zone, target at a fixed
//! multiple of the risk. Higher-timeframe bias is not used in v1.

use crate::detect::{DetectConfig, Event, EventKind};
use crate::risk::{Decision, Proposal, RiskConfig, evaluate};
use crate::sim::{OpenOrder, Sim};
use crate::types::{Candle, Dir, PairSpec};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const REJECT_KEY_LEN: usize = 48;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StrategyConfig {
    pub swing_len: usize,
    pub close_break: bool,
    pub rr: f64,
    pub stop_buffer_pips: f64,
    /// A setup that is not touched within this many bars is dropped.
    pub setup_ttl_bars: usize,
    /// Trade fair value gaps as setups.
    pub use_fvg: bool,
    /// Trade order blocks as setups.
    pub use_order_blocks: bool,
}

impl StrategyConfig {
    pub fn detect_cfg(&self) -> DetectConfig {
        DetectConfig {
            swing_len: self.swing_len,
            close_break: self.close_break,
        }
    }
}

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            swing_len: DetectConfig::default().swing_len,
            close_break: DetectConfig::default().close_break,
            rr: 2.0,
            stop_buffer_pips: 1.0,
            setup_ttl_bars: 48,
            use_fvg: true,
            use_order_blocks: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Setup {
    id: usize,
    dir: Dir,
    top: f64,
    bottom: f64,
    created_at: usize,
    label: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ZoneView {
    pub dir: Dir,
    pub label: String,
    pub top: f64,
    pub bottom: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub dir: Dir,
    pub stop: f64,
    pub rr: f64,
    pub note: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Strategy {
    trend: Option<Dir>,
    setups: Vec<Setup>,
}

impl Strategy {
    pub fn trend(&self) -> Option<Dir> {
        self.trend
    }

    pub fn setup_count(&self) -> usize {
        self.setups.len()
    }

    /// The live setups as plain data, for the overview screen.
    pub fn zones(&self) -> Vec<ZoneView> {
        self.setups
            .iter()
            .map(|s| ZoneView {
                dir: s.dir,
                label: s.label.clone(),
                top: s.top,
                bottom: s.bottom,
            })
            .collect()
    }

    /// One line per live setup, for the chat.
    pub fn describe(&self) -> String {
        let trend = match self.trend {
            Some(Dir::Bull) => "bullish",
            Some(Dir::Bear) => "bearish",
            None => "undetermined",
        };
        let mut out = format!("trend {trend}, {} live setup(s)", self.setups.len());
        for s in &self.setups {
            out.push_str(&format!(
                "
  {:?} {} {:.5} to {:.5}",
                s.dir, s.label, s.bottom, s.top
            ));
        }
        out
    }

    /// Feeds the events that became known on bar `t` and returns an entry to take at the next open.
    pub fn on_bar(
        &mut self,
        cfg: &StrategyConfig,
        spec: &PairSpec,
        t: usize,
        c: &Candle,
        events: &[Event],
    ) -> Option<Entry> {
        let mut touched = Vec::new();
        for ev in events {
            match &ev.kind {
                EventKind::Break { dir, .. } => self.set_trend(*dir),
                EventKind::OrderBlock {
                    id,
                    dir,
                    top,
                    bottom,
                    ..
                } if cfg.use_order_blocks => {
                    self.add_setup(*id, *dir, *top, *bottom, t, "order block");
                }
                EventKind::Fvg {
                    id,
                    dir,
                    top,
                    bottom,
                } if cfg.use_fvg => {
                    self.add_setup(*id, *dir, *top, *bottom, t, "fair value gap");
                }
                EventKind::Mitigated { id } => self.setups.retain(|s| s.id != *id),
                EventKind::Touched { id } => touched.push(*id),
                _ => {}
            }
        }
        self.setups
            .retain(|s| t.saturating_sub(s.created_at) <= cfg.setup_ttl_bars);
        touched
            .into_iter()
            .find_map(|id| self.entry_from_touch(cfg, spec, id, c))
    }

    fn set_trend(&mut self, dir: Dir) {
        self.trend = Some(dir);
        self.setups.retain(|s| s.dir == dir);
    }

    fn add_setup(&mut self, id: usize, dir: Dir, top: f64, bottom: f64, t: usize, label: &str) {
        if self.trend == Some(dir) {
            self.setups.push(Setup {
                id,
                dir,
                top,
                bottom,
                created_at: t,
                label: label.to_string(),
            });
        }
    }

    fn entry_from_touch(
        &mut self,
        cfg: &StrategyConfig,
        spec: &PairSpec,
        id: usize,
        c: &Candle,
    ) -> Option<Entry> {
        let i = self.setups.iter().position(|s| s.id == id)?;
        let s = self.setups.remove(i);
        if self.trend != Some(s.dir) {
            return None;
        }
        let held = match s.dir {
            Dir::Bull => c.close >= s.bottom,
            Dir::Bear => c.close <= s.top,
        };
        if !held {
            return None;
        }
        let buffer = cfg.stop_buffer_pips * spec.pip;
        let stop = match s.dir {
            Dir::Bull => s.bottom - buffer,
            Dir::Bear => s.top + buffer,
        };
        Some(Entry {
            dir: s.dir,
            stop,
            rr: cfg.rr,
            note: format!("{} retest", s.label),
        })
    }
}

/// Strategy, paper broker and risk engine wired together.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Driver {
    pub strategy: Strategy,
    pub risk: RiskConfig,
    pub cfg: StrategyConfig,
    pending: Option<Entry>,
    pub rejects: BTreeMap<String, usize>,
}

impl Driver {
    pub fn new(cfg: StrategyConfig, risk: RiskConfig) -> Self {
        Self {
            strategy: Strategy::default(),
            risk,
            cfg,
            pending: None,
            rejects: BTreeMap::new(),
        }
    }

    /// Keeps the learned market structure but drops any entry waiting for the next open, so
    /// history replayed to warm up the detectors never turns into a paper trade.
    pub fn reset_pending(&mut self) {
        self.pending = None;
        self.rejects.clear();
    }

    /// Rebuilds the market view from candles that were already processed, without trading, and
    /// returns the index of the first candle after `until_ts`.
    ///
    /// Detector output is numbered from the start of the window it ran over, so a stored view
    /// goes stale when the window slides. Replaying the window each time keeps every zone id and
    /// age on one timeline, and gives the same view a single uninterrupted run would.
    pub fn rebuild_strategy(
        &mut self,
        spec: &PairSpec,
        candles: &[Candle],
        by_bar: &[Vec<Event>],
        until_ts: i64,
    ) -> usize {
        self.strategy = Strategy::default();
        self.pending = None;
        let mut next = 0;
        for (t, c) in candles.iter().enumerate() {
            if c.ts > until_ts {
                break;
            }
            self.pending = self.strategy.on_bar(&self.cfg, spec, t, c, &by_bar[t]);
            next = t + 1;
        }
        next
    }

    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Processes closed candle `c` (index `t`) with the events that became known on it. Returns
    /// log lines for anything worth telling the user.
    pub fn on_bar(
        &mut self,
        sim: &mut Sim,
        spec: &PairSpec,
        t: usize,
        c: &Candle,
        events: &[Event],
    ) -> Vec<String> {
        let mut log = Vec::new();
        sim.advance_day(c.ts);
        if let Some(entry) = self.pending.take() {
            log.extend(self.try_open(sim, spec, c, entry));
        }
        let before = sim.trades.len();
        sim.on_candle(spec, c);
        for tr in &sim.trades[before..] {
            log.push(format!(
                "closed {:?} {} {:?} at {:.5}: {:+.2} R ({:+.2} USD)",
                tr.dir, tr.pair, tr.reason, tr.exit, tr.r, tr.pnl_usd
            ));
        }
        self.pending = self.strategy.on_bar(&self.cfg, spec, t, c, events);
        if let Some(e) = &self.pending {
            log.push(format!(
                "setup ready: {:?} {} ({}), entry at next open, stop {:.5}",
                e.dir, spec.name, e.note, e.stop
            ));
        }
        log
    }

    fn try_open(&mut self, sim: &mut Sim, spec: &PairSpec, c: &Candle, e: Entry) -> Option<String> {
        let s = e.dir.sign();
        let risk_dist = (c.open - e.stop) * s;
        if risk_dist <= 0.0 {
            return Some(self.reject("stop already behind the open"));
        }
        let target = c.open + s * e.rr * risk_dist;
        let spread = sim.cfg.spread_for(spec);
        let proposal = Proposal {
            spec: *spec,
            dir: e.dir,
            entry: c.open,
            stop: e.stop,
            target,
            spread_pips: spread,
            now: c.ts,
        };
        match evaluate(&self.risk, &sim.account_view(), &proposal) {
            Decision::Approve {
                units,
                risk_usd,
                stop_pips,
                reward_risk,
            } => {
                let order = OpenOrder {
                    dir: e.dir,
                    units,
                    mid: c.open,
                    stop: e.stop,
                    target,
                    ts: c.ts,
                    note: &e.note,
                };
                sim.open(spec, order);
                Some(format!(
                    "opened {:?} {} {} units, stop {:.1} pips, {:.1}R target, risking {:.2} USD",
                    e.dir, spec.name, units, stop_pips, reward_risk, risk_usd
                ))
            }
            Decision::Reject { reason } => Some(self.reject(&reason)),
        }
    }

    fn reject(&mut self, reason: &str) -> String {
        let plain: String = reason
            .chars()
            .filter(|c| !c.is_ascii_digit() && !matches!(c, '.' | '%'))
            .collect();
        let key: String = plain
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(REJECT_KEY_LEN)
            .collect();
        *self.rejects.entry(key).or_insert(0) += 1;
        format!("skipped: {reason}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backtest::group_by_known_at;
    use crate::detect::analyze;
    use crate::risk::RiskConfig;
    use crate::sim::SimConfig;
    use crate::types::pair_spec;

    fn series(n: usize, seed: u64) -> Vec<Candle> {
        let mut x = seed;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % 2_000) as f64 / 1_000.0 - 1.0
        };
        let mut price = 1.1;
        (0..n)
            .map(|i| {
                let open = price;
                price += next() * 0.002;
                let close = price;
                Candle {
                    ts: i as i64 * 3_600,
                    open,
                    high: open.max(close) + (next().abs() * 0.0008),
                    low: open.min(close) - (next().abs() * 0.0008),
                    close,
                }
            })
            .collect()
    }

    #[test]
    fn a_rebuilt_view_matches_an_uninterrupted_run() {
        let spec = pair_spec("EURUSD").unwrap();
        let candles = series(700, 7);
        let cfg = StrategyConfig::default();
        let by_bar = group_by_known_at(analyze(&candles, &cfg.detect_cfg()), candles.len());

        let mut full = Driver::new(cfg.clone(), RiskConfig::default());
        let mut sim = Sim::new(SimConfig::default(), 10_000.0);
        for (t, c) in candles.iter().enumerate().take(400) {
            full.on_bar(&mut sim, &spec, t, c, &by_bar[t]);
        }

        let mut rebuilt = Driver::new(cfg, RiskConfig::default());
        let next = rebuilt.rebuild_strategy(&spec, &candles, &by_bar, candles[399].ts);
        assert_eq!(next, 400);
        assert_eq!(
            serde_json::to_string(&rebuilt.strategy).unwrap(),
            serde_json::to_string(&full.strategy).unwrap()
        );
        assert_eq!(rebuilt.has_pending(), full.has_pending());
    }

    #[test]
    fn setups_expire_on_one_timeline() {
        let spec = pair_spec("EURUSD").unwrap();
        let candles = series(900, 11);
        let cfg = StrategyConfig::default();
        let by_bar = group_by_known_at(analyze(&candles, &cfg.detect_cfg()), candles.len());
        let mut d = Driver::new(cfg.clone(), RiskConfig::default());
        d.rebuild_strategy(&spec, &candles, &by_bar, i64::MAX);
        let limit = cfg.setup_ttl_bars;
        assert!(
            d.strategy.zones().len() <= limit,
            "live setups are capped by the expiry window"
        );
    }
}
