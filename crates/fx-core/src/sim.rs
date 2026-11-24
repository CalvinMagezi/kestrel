//! Paper broker. One engine serves both the backtester and the live paper loop, so a strategy sees
//! identical fills in both. Candles are mid prices: entries pay half the spread plus slippage,
//! stop exits pay the same, target exits pay half the spread only.
//!
//! Intrabar rule: when a candle's range touches both the stop and the target, the stop is taken.
//! A candle that opens beyond a level fills at the open, not the level.

use crate::risk::{AccountView, OpenPosition};
use crate::types::{Candle, Dir, PairSpec, pair_spec};
use serde::{Deserialize, Serialize};

const SECS_PER_DAY: i64 = 86_400;
/// FX rollover happens at 22:00 UTC, when the broker day changes.
const ROLLOVER_OFFSET_SECS: i64 = 22 * 3_600;
const STANDARD_LOT: f64 = 100_000.0;
const WEDNESDAY: i64 = 2;
const LAST_WEEKDAY: i64 = 4;
const TRIPLE_SWAP: f64 = 3.0;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SimConfig {
    /// Fixed spread for every pair. `None` uses each pair's default spread.
    pub spread_pips: Option<f64>,
    pub slippage_pips: f64,
    pub swap_enabled: bool,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            spread_pips: None,
            slippage_pips: 0.3,
            swap_enabled: true,
        }
    }
}

impl SimConfig {
    pub fn spread_for(&self, spec: &PairSpec) -> f64 {
        self.spread_pips.unwrap_or(spec.default_spread_pips)
    }
}

/// Illustrative swap in USD per standard lot per night (long, short). Real swaps vary by broker.
fn swap_per_lot(pair: &str, dir: Dir) -> f64 {
    let (long, short) = match pair {
        "EURUSD" => (-6.5, 3.5),
        "GBPUSD" => (-3.0, -1.0),
        "USDJPY" => (8.0, -12.0),
        "USDCHF" => (4.0, -8.0),
        "AUDUSD" => (-2.0, -1.0),
        "USDCAD" => (2.0, -5.0),
        "NZDUSD" => (-1.5, -2.0),
        _ => (-1.0, -1.0),
    };
    match dir {
        Dir::Bull => long,
        Dir::Bear => short,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitReason {
    Stop,
    Target,
    Manual,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub id: u64,
    pub pair: String,
    pub dir: Dir,
    pub units: f64,
    pub entry: f64,
    pub entry_ts: i64,
    pub stop: f64,
    pub target: f64,
    pub risk_usd: f64,
    pub stop_pips: f64,
    pub swap_usd: f64,
    last_roll_day: i64,
    /// Free-form note, for example the setup that triggered the entry.
    pub note: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Trade {
    pub id: u64,
    pub pair: String,
    pub dir: Dir,
    pub units: f64,
    pub entry_ts: i64,
    pub exit_ts: i64,
    pub entry: f64,
    pub exit: f64,
    pub pips: f64,
    pub stop_pips: f64,
    pub pnl_usd: f64,
    pub swap_usd: f64,
    /// Result in multiples of the initial risk, swap included.
    pub r: f64,
    pub reason: ExitReason,
    pub note: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sim {
    pub cfg: SimConfig,
    /// Realised equity.
    pub equity: f64,
    pub positions: Vec<Position>,
    pub trades: Vec<Trade>,
    next_id: u64,
    day: i64,
    day_start_equity: f64,
    pub kill_switch: bool,
    /// What the account started with, for the change shown to the owner.
    #[serde(default = "default_start_equity")]
    pub start_equity: f64,
}

fn default_start_equity() -> f64 {
    10_000.0
}

fn roll_day(ts: i64) -> i64 {
    (ts - ROLLOVER_OFFSET_SECS).div_euclid(SECS_PER_DAY)
}

fn to_usd(spec: &PairSpec, quote_amount: f64, price: f64) -> f64 {
    if spec.usd_is_base {
        quote_amount / price
    } else {
        quote_amount
    }
}

impl Sim {
    pub fn new(cfg: SimConfig, equity: f64) -> Self {
        Self {
            cfg,
            equity,
            positions: Vec::new(),
            trades: Vec::new(),
            next_id: 1,
            day: i64::MIN,
            day_start_equity: equity,
            kill_switch: false,
            start_equity: equity,
        }
    }

    /// Opens a position at the mid price `mid`, paying spread and slippage against the entry.
    pub fn open(&mut self, spec: &PairSpec, p: OpenOrder<'_>) -> u64 {
        let cost = (self.cfg.spread_for(spec) / 2.0 + self.cfg.slippage_pips) * spec.pip;
        let entry = p.mid + p.dir.sign() * cost;
        let risk_quote = (entry - p.stop).abs() * p.units;
        let id = self.next_id;
        self.next_id += 1;
        self.positions.push(Position {
            id,
            pair: spec.name.to_string(),
            dir: p.dir,
            units: p.units,
            entry,
            entry_ts: p.ts,
            stop: p.stop,
            target: p.target,
            risk_usd: to_usd(spec, risk_quote, entry),
            stop_pips: (entry - p.stop).abs() / spec.pip,
            swap_usd: 0.0,
            last_roll_day: roll_day(p.ts),
            note: p.note.to_string(),
        });
        id
    }

    /// Advances one candle of `spec`: swap, then stop and target checks.
    pub fn on_candle(&mut self, spec: &PairSpec, c: &Candle) {
        self.roll_account_day(c.ts);
        if self.cfg.swap_enabled {
            self.apply_swap(spec, c.ts);
        }
        let mut exits = Vec::new();
        for (i, pos) in self.positions.iter().enumerate() {
            if pos.pair == spec.name
                && let Some(x) = self.check_exit(spec, pos, c)
            {
                exits.push((i, x));
            }
        }
        for (i, (price, reason)) in exits.into_iter().rev() {
            let pos = self.positions.remove(i);
            self.close(spec, pos, price, c.ts, reason);
        }
    }

    pub fn close_manual(&mut self, spec: &PairSpec, id: u64, mid: f64, ts: i64) -> bool {
        let Some(i) = self
            .positions
            .iter()
            .position(|p| p.id == id && p.pair == spec.name)
        else {
            return false;
        };
        let pos = self.positions.remove(i);
        let cost = self.cfg.spread_for(spec) / 2.0 * spec.pip;
        self.close(
            spec,
            pos.clone(),
            mid - pos.dir.sign() * cost,
            ts,
            ExitReason::Manual,
        );
        true
    }

    fn check_exit(&self, spec: &PairSpec, pos: &Position, c: &Candle) -> Option<(f64, ExitReason)> {
        let half = self.cfg.spread_for(spec) / 2.0 * spec.pip;
        let stop_cost = half + self.cfg.slippage_pips * spec.pip;
        let s = pos.dir.sign();
        let beyond = |level: f64, price: f64| (price - level) * s;
        // Positive means price is on the profit side of `level`.
        let stop_gap = beyond(pos.stop, c.open) <= 0.0;
        let stop_hit = match pos.dir {
            Dir::Bull => c.low <= pos.stop,
            Dir::Bear => c.high >= pos.stop,
        };
        let target_hit = match pos.dir {
            Dir::Bull => c.high >= pos.target,
            Dir::Bear => c.low <= pos.target,
        };
        if stop_gap {
            return Some((c.open - s * stop_cost, ExitReason::Stop));
        }
        if stop_hit {
            return Some((pos.stop - s * stop_cost, ExitReason::Stop));
        }
        if target_hit {
            let fill = if beyond(pos.target, c.open) >= 0.0 {
                c.open
            } else {
                pos.target
            };
            return Some((fill - s * half, ExitReason::Target));
        }
        None
    }

    fn close(&mut self, spec: &PairSpec, pos: Position, exit: f64, ts: i64, reason: ExitReason) {
        let s = pos.dir.sign();
        let pnl_quote = (exit - pos.entry) * s * pos.units;
        let price_pnl = to_usd(spec, pnl_quote, exit);
        let pnl_usd = price_pnl + pos.swap_usd;
        let r = if pos.risk_usd > 0.0 {
            pnl_usd / pos.risk_usd
        } else {
            0.0
        };
        self.equity += pnl_usd;
        self.trades.push(Trade {
            id: pos.id,
            pair: pos.pair,
            dir: pos.dir,
            units: pos.units,
            entry_ts: pos.entry_ts,
            exit_ts: ts,
            entry: pos.entry,
            exit,
            pips: (exit - pos.entry) * s / spec.pip,
            stop_pips: pos.stop_pips,
            pnl_usd,
            swap_usd: pos.swap_usd,
            r,
            reason,
            note: pos.note,
        });
    }

    fn apply_swap(&mut self, spec: &PairSpec, ts: i64) {
        let today = roll_day(ts);
        for pos in self.positions.iter_mut().filter(|p| p.pair == spec.name) {
            for day in (pos.last_roll_day + 1)..=today {
                let weekday = (day + 3).rem_euclid(7);
                if weekday > LAST_WEEKDAY {
                    continue;
                }
                let mult = if weekday == WEDNESDAY {
                    TRIPLE_SWAP
                } else {
                    1.0
                };
                let amount = pos.units / STANDARD_LOT * swap_per_lot(spec.name, pos.dir) * mult;
                pos.swap_usd += amount;
                self.equity += amount;
            }
            pos.last_roll_day = pos.last_roll_day.max(today);
        }
    }

    /// Starts a new UTC day for risk limits. Call it before judging an entry on candle `ts`, so the
    /// first entry of a day is not held to yesterday's loss streak.
    pub fn advance_day(&mut self, ts: i64) {
        self.roll_account_day(ts);
    }

    fn roll_account_day(&mut self, ts: i64) {
        let d = ts.div_euclid(SECS_PER_DAY);
        // Candles of several pairs reach one account out of order, so the day only moves forward.
        if d > self.day {
            self.day = d;
            self.day_start_equity = self.equity;
        }
    }

    /// Losing streak among trades closed today (UTC). The halt it triggers ends at the next day, so
    /// a bot that stops trading cannot stay stopped forever waiting for a win it will never take.
    pub fn consecutive_losses(&self) -> usize {
        let day_start = self.day.saturating_mul(SECS_PER_DAY);
        self.trades
            .iter()
            .rev()
            .take_while(|t| t.exit_ts >= day_start && t.pnl_usd < 0.0)
            .count()
    }

    /// Risk engine view. Equity is realised equity, so the daily loss limit counts closed trades.
    pub fn account_view(&self) -> AccountView {
        AccountView {
            equity: self.equity,
            day_start_equity: self.day_start_equity,
            open: self
                .positions
                .iter()
                .map(|p| OpenPosition {
                    pair: p.pair.clone(),
                    dir: p.dir,
                })
                .collect(),
            consecutive_losses: self.consecutive_losses(),
            kill_switch: self.kill_switch,
        }
    }

    /// Equity including open positions marked at `mid` for each pair.
    pub fn marked_equity(&self, marks: &[(String, f64)]) -> f64 {
        let open: f64 = self
            .positions
            .iter()
            .filter_map(|p| {
                let mid = marks.iter().find(|(n, _)| *n == p.pair)?.1;
                let spec = pair_spec(&p.pair)?;
                Some(to_usd(&spec, (mid - p.entry) * p.dir.sign() * p.units, mid))
            })
            .sum();
        self.equity + open
    }
}

pub struct OpenOrder<'a> {
    pub dir: Dir,
    pub units: f64,
    pub mid: f64,
    pub stop: f64,
    pub target: f64,
    pub ts: i64,
    pub note: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eurusd() -> PairSpec {
        pair_spec("EURUSD").unwrap()
    }

    fn candle(ts: i64, o: f64, h: f64, l: f64, c: f64) -> Candle {
        Candle {
            ts,
            open: o,
            high: h,
            low: l,
            close: c,
        }
    }

    fn no_cost() -> SimConfig {
        SimConfig {
            spread_pips: Some(0.0),
            slippage_pips: 0.0,
            swap_enabled: false,
        }
    }

    fn long(sim: &mut Sim, ts: i64) {
        let o = OpenOrder {
            dir: Dir::Bull,
            units: 10_000.0,
            mid: 1.1000,
            stop: 1.0950,
            target: 1.1100,
            ts,
            note: "",
        };
        sim.open(&eurusd(), o);
    }

    #[test]
    fn target_hit_pays_two_r_without_costs() {
        let mut sim = Sim::new(no_cost(), 10_000.0);
        long(&mut sim, 0);
        sim.on_candle(&eurusd(), &candle(900, 1.1000, 1.1120, 1.0990, 1.1110));
        let t = &sim.trades[0];
        assert_eq!(t.reason, ExitReason::Target);
        assert!((t.r - 2.0).abs() < 1e-9, "r was {}", t.r);
        assert!((sim.equity - 10_100.0).abs() < 1e-6);
    }

    #[test]
    fn candle_touching_both_levels_takes_the_stop() {
        let mut sim = Sim::new(no_cost(), 10_000.0);
        long(&mut sim, 0);
        sim.on_candle(&eurusd(), &candle(900, 1.1000, 1.1120, 1.0940, 1.1000));
        assert_eq!(sim.trades[0].reason, ExitReason::Stop);
        assert!((sim.trades[0].r + 1.0).abs() < 1e-9);
    }

    #[test]
    fn gap_through_stop_fills_at_the_open() {
        let mut sim = Sim::new(no_cost(), 10_000.0);
        long(&mut sim, 0);
        sim.on_candle(&eurusd(), &candle(900, 1.0900, 1.0910, 1.0890, 1.0900));
        assert!((sim.trades[0].exit - 1.0900).abs() < 1e-12);
        assert!(sim.trades[0].r < -1.5);
    }

    #[test]
    fn spread_and_slippage_worsen_entry_and_stop_exit() {
        let cfg = SimConfig {
            spread_pips: Some(1.0),
            slippage_pips: 0.3,
            swap_enabled: false,
        };
        let mut sim = Sim::new(cfg, 10_000.0);
        long(&mut sim, 0);
        assert!((sim.positions[0].entry - 1.10008).abs() < 1e-9);
        sim.on_candle(&eurusd(), &candle(900, 1.1000, 1.1005, 1.0940, 1.0950));
        assert!((sim.trades[0].exit - (1.0950 - 0.00008)).abs() < 1e-9);
    }

    #[test]
    fn usdjpy_pnl_converts_to_usd() {
        let spec = pair_spec("USDJPY").unwrap();
        let mut sim = Sim::new(no_cost(), 10_000.0);
        let o = OpenOrder {
            dir: Dir::Bull,
            units: 10_000.0,
            mid: 150.0,
            stop: 149.5,
            target: 151.0,
            ts: 0,
            note: "",
        };
        sim.open(&spec, o);
        sim.on_candle(&spec, &candle(900, 150.0, 151.2, 149.9, 151.0));
        // 1.00 yen * 10_000 units / 151.0 = 66.2251... USD
        assert!((sim.trades[0].pnl_usd - 10_000.0 / 151.0).abs() < 1e-6);
    }

    #[test]
    fn wednesday_rollover_pays_triple_swap() {
        let cfg = SimConfig {
            spread_pips: Some(0.0),
            slippage_pips: 0.0,
            swap_enabled: true,
        };
        let mut sim = Sim::new(cfg, 10_000.0);
        // 2024-01-03 is a Wednesday. Open at 12:00 UTC, then see a candle after the 22:00 rollover.
        let wed_noon = 1_704_283_200;
        long(&mut sim, wed_noon);
        sim.on_candle(
            &eurusd(),
            &candle(wed_noon + 11 * 3_600, 1.1, 1.1001, 1.0999, 1.1),
        );
        let expected = 10_000.0 / STANDARD_LOT * -6.5 * 3.0;
        assert!((sim.positions[0].swap_usd - expected).abs() < 1e-9);
    }

    #[test]
    fn weekend_rollovers_are_skipped() {
        let cfg = SimConfig {
            spread_pips: Some(0.0),
            slippage_pips: 0.0,
            swap_enabled: true,
        };
        let mut sim = Sim::new(cfg, 10_000.0);
        // Friday 2024-01-05 23:00 UTC open (after rollover), next candle Monday 2024-01-08 00:00.
        let fri = 1_704_495_600;
        long(&mut sim, fri);
        sim.on_candle(
            &eurusd(),
            &candle(fri + 2 * 3_600 + 2 * SECS_PER_DAY, 1.1, 1.1001, 1.0999, 1.1),
        );
        assert_eq!(sim.positions[0].swap_usd, 0.0);
    }

    #[test]
    fn short_target_and_consecutive_losses() {
        let mut sim = Sim::new(no_cost(), 10_000.0);
        let o = OpenOrder {
            dir: Dir::Bear,
            units: 10_000.0,
            mid: 1.1,
            stop: 1.105,
            target: 1.09,
            ts: 0,
            note: "",
        };
        sim.open(&eurusd(), o);
        sim.on_candle(&eurusd(), &candle(900, 1.1, 1.1060, 1.0990, 1.1));
        assert_eq!(sim.consecutive_losses(), 1);
        // A candle on the next UTC day starts a fresh streak.
        sim.on_candle(
            &eurusd(),
            &candle(SECS_PER_DAY + 900, 1.1, 1.1001, 1.0999, 1.1),
        );
        assert_eq!(sim.consecutive_losses(), 0);
    }

    #[test]
    fn candles_arriving_out_of_order_do_not_reset_the_day() {
        let mut sim = Sim::new(no_cost(), 10_000.0);
        sim.on_candle(
            &eurusd(),
            &candle(2 * SECS_PER_DAY, 1.1, 1.1001, 1.0999, 1.1),
        );
        sim.equity = 9_900.0;
        // An older candle from another pair must not start a new "today" at the lower equity.
        sim.on_candle(&eurusd(), &candle(SECS_PER_DAY, 1.1, 1.1001, 1.0999, 1.1));
        assert_eq!(sim.account_view().day_start_equity, 10_000.0);
    }
}
