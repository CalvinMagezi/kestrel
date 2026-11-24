//! Trade statistics and the checks that stop a lucky run being mistaken for an edge.
//! Adapted from vibe-trading's validation module, but on trade R-multiples: shuffling trade order
//! leaves total profit unchanged, so the checks here resample trades instead.

use crate::sim::Trade;
use serde::{Deserialize, Serialize};

const DEFAULT_RESAMPLES: usize = 2_000;
const DEFAULT_CONFIDENCE: f64 = 0.95;
const MIN_TRADES_FOR_CI: usize = 5;

/// xorshift64*, enough for resampling and reproducible from a seed.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n.max(1) as u64) as usize
    }

    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub trades: usize,
    pub win_rate: f64,
    pub expectancy_r: f64,
    pub avg_win_r: f64,
    pub avg_loss_r: f64,
    pub profit_factor: f64,
    pub total_pnl_usd: f64,
    pub max_drawdown_pct: f64,
}

pub fn summarize(trades: &[Trade], initial_equity: f64) -> Summary {
    if trades.is_empty() {
        return Summary::default();
    }
    let n = trades.len() as f64;
    let wins: Vec<f64> = trades.iter().filter(|t| t.r > 0.0).map(|t| t.r).collect();
    let losses: Vec<f64> = trades.iter().filter(|t| t.r <= 0.0).map(|t| t.r).collect();
    let gross_win: f64 = trades
        .iter()
        .filter(|t| t.pnl_usd > 0.0)
        .map(|t| t.pnl_usd)
        .sum();
    let gross_loss: f64 = trades
        .iter()
        .filter(|t| t.pnl_usd < 0.0)
        .map(|t| -t.pnl_usd)
        .sum();
    let mean = |v: &[f64]| {
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    };
    let pnls: Vec<f64> = trades.iter().map(|t| t.pnl_usd).collect();
    Summary {
        trades: trades.len(),
        win_rate: wins.len() as f64 / n,
        expectancy_r: trades.iter().map(|t| t.r).sum::<f64>() / n,
        avg_win_r: mean(&wins),
        avg_loss_r: mean(&losses),
        profit_factor: if gross_loss > 0.0 {
            gross_win / gross_loss
        } else {
            f64::INFINITY
        },
        total_pnl_usd: pnls.iter().sum(),
        max_drawdown_pct: max_drawdown_pct(&pnls, initial_equity),
    }
}

pub fn max_drawdown_pct(pnls: &[f64], initial_equity: f64) -> f64 {
    let (mut equity, mut peak, mut worst) = (initial_equity, initial_equity, 0.0_f64);
    for p in pnls {
        equity += p;
        peak = peak.max(equity);
        if peak > 0.0 {
            worst = worst.max((peak - equity) / peak * 100.0);
        }
    }
    worst
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Interval {
    pub low: f64,
    pub high: f64,
    pub prob_positive: f64,
}

/// Bootstrap confidence interval of mean R. `None` with fewer than five trades.
pub fn bootstrap_expectancy(
    rs: &[f64],
    resamples: usize,
    confidence: f64,
    seed: u64,
) -> Option<Interval> {
    if rs.len() < MIN_TRADES_FOR_CI || resamples == 0 {
        return None;
    }
    let mut rng = Rng::new(seed);
    let mut means: Vec<f64> = (0..resamples)
        .map(|_| (0..rs.len()).map(|_| rs[rng.below(rs.len())]).sum::<f64>() / rs.len() as f64)
        .collect();
    means.sort_by(f64::total_cmp);
    let tail = (1.0 - confidence) / 2.0;
    let at = |q: f64| means[((q * (means.len() - 1) as f64).round() as usize).min(means.len() - 1)];
    let positive = means.iter().filter(|m| **m > 0.0).count() as f64 / means.len() as f64;
    Some(Interval {
        low: at(tail),
        high: at(1.0 - tail),
        prob_positive: positive,
    })
}

pub fn default_bootstrap(rs: &[f64], seed: u64) -> Option<Interval> {
    bootstrap_expectancy(rs, DEFAULT_RESAMPLES, DEFAULT_CONFIDENCE, seed)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DrawdownDist {
    pub median_pct: f64,
    pub p95_pct: f64,
}

/// Max drawdown distribution when `rs` are redrawn with replacement and each trade risks
/// `risk_pct` of current equity. Shows how bad a normal run of luck can look.
pub fn monte_carlo_drawdown(
    rs: &[f64],
    risk_pct: f64,
    runs: usize,
    seed: u64,
) -> Option<DrawdownDist> {
    if rs.len() < MIN_TRADES_FOR_CI || runs == 0 {
        return None;
    }
    let mut rng = Rng::new(seed);
    let mut dds: Vec<f64> = (0..runs)
        .map(|_| {
            let (mut eq, mut peak, mut worst) = (1.0_f64, 1.0_f64, 0.0_f64);
            for _ in 0..rs.len() {
                eq *= 1.0 + risk_pct / 100.0 * rs[rng.below(rs.len())];
                peak = peak.max(eq);
                worst = worst.max((peak - eq) / peak * 100.0);
            }
            worst
        })
        .collect();
    dds.sort_by(f64::total_cmp);
    let at = |q: f64| dds[((q * (dds.len() - 1) as f64).round() as usize).min(dds.len() - 1)];
    Some(DrawdownDist {
        median_pct: at(0.5),
        p95_pct: at(0.95),
    })
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowStat {
    pub trades: usize,
    pub expectancy_r: f64,
}

/// Splits trades (in time order) into `windows` equal groups. A real edge shows the same sign in
/// most windows; one great window and several flat ones is luck.
pub fn walk_forward(trades: &[Trade], windows: usize) -> Vec<WindowStat> {
    if windows == 0 || trades.len() < windows {
        return Vec::new();
    }
    let size = trades.len() / windows;
    (0..windows)
        .map(|w| {
            let end = if w + 1 == windows {
                trades.len()
            } else {
                (w + 1) * size
            };
            let slice = &trades[w * size..end];
            WindowStat {
                trades: slice.len(),
                expectancy_r: slice.iter().map(|t| t.r).sum::<f64>() / slice.len() as f64,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::ExitReason;
    use crate::types::Dir;

    fn trade(r: f64, pnl: f64) -> Trade {
        Trade {
            id: 1,
            pair: "EURUSD".into(),
            dir: Dir::Bull,
            units: 1.0,
            entry_ts: 0,
            exit_ts: 1,
            entry: 1.0,
            exit: 1.0,
            pips: 0.0,
            stop_pips: 10.0,
            pnl_usd: pnl,
            swap_usd: 0.0,
            r,
            reason: ExitReason::Target,
            note: String::new(),
        }
    }

    #[test]
    fn summary_matches_hand_calculation() {
        let t = [
            trade(2.0, 200.0),
            trade(-1.0, -100.0),
            trade(-1.0, -100.0),
            trade(2.0, 200.0),
        ];
        let s = summarize(&t, 10_000.0);
        assert_eq!(s.trades, 4);
        assert!((s.win_rate - 0.5).abs() < 1e-12);
        assert!((s.expectancy_r - 0.5).abs() < 1e-12);
        assert!((s.profit_factor - 2.0).abs() < 1e-12);
        assert!((s.max_drawdown_pct - 200.0 / 10_200.0 * 100.0).abs() < 1e-9);
    }

    #[test]
    fn empty_input_is_safe() {
        assert_eq!(summarize(&[], 100.0), Summary::default());
        assert!(default_bootstrap(&[1.0], 1).is_none());
        assert!(walk_forward(&[], 3).is_empty());
    }

    #[test]
    fn bootstrap_interval_straddles_zero_for_a_coin_flip() {
        let rs: Vec<f64> = (0..40)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        let ci = default_bootstrap(&rs, 7).unwrap();
        assert!(ci.low < 0.0 && ci.high > 0.0, "{ci:?}");
    }

    #[test]
    fn bootstrap_interval_excludes_zero_for_a_clear_edge() {
        let rs: Vec<f64> = (0..60)
            .map(|i| if i % 5 == 0 { -1.0 } else { 1.5 })
            .collect();
        let ci = default_bootstrap(&rs, 7).unwrap();
        assert!(ci.low > 0.0 && ci.prob_positive > 0.99, "{ci:?}");
    }

    #[test]
    fn same_seed_is_reproducible() {
        let rs = [1.0, -1.0, 2.0, -1.0, 0.5, -1.0];
        assert_eq!(default_bootstrap(&rs, 3), default_bootstrap(&rs, 3));
        assert_eq!(
            monte_carlo_drawdown(&rs, 1.0, 200, 3),
            monte_carlo_drawdown(&rs, 1.0, 200, 3)
        );
    }

    #[test]
    fn drawdown_p95_is_not_below_median() {
        let rs = [1.0, -1.0, 2.0, -1.0, 0.5, -1.0, 1.0, -1.0];
        let d = monte_carlo_drawdown(&rs, 1.0, 500, 9).unwrap();
        assert!(d.p95_pct >= d.median_pct);
    }

    #[test]
    fn walk_forward_covers_all_trades() {
        let t: Vec<Trade> = (0..10).map(|i| trade(i as f64, 0.0)).collect();
        let w = walk_forward(&t, 3);
        assert_eq!(w.iter().map(|x| x.trades).sum::<usize>(), 10);
    }
}
