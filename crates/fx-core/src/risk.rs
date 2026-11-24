//! Risk engine. See `briefs/02-risk.md`. Pure functions: the caller passes the clock and state.

use crate::types::{Dir, PairSpec, pair_spec};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RiskConfig {
    pub risk_per_trade_pct: f64,
    pub max_daily_loss_pct: f64,
    pub max_open_positions: usize,
    pub max_spread_pips: f64,
    pub min_stop_pips: f64,
    pub max_stop_pips: f64,
    pub min_reward_risk: f64,
    pub max_units: f64,
    /// Number of consecutive losing closed trades after which the engine halts new entries.
    pub max_consecutive_losses: usize,
    /// Half-open `[start, end)` unix-second windows in which no new entries are allowed.
    pub blackouts: Vec<(i64, i64)>,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            risk_per_trade_pct: 0.5,
            max_daily_loss_pct: 2.0,
            max_open_positions: 3,
            max_spread_pips: 3.0,
            min_stop_pips: 5.0,
            max_stop_pips: 150.0,
            min_reward_risk: 1.5,
            max_units: 200_000.0,
            max_consecutive_losses: 4,
            blackouts: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OpenPosition {
    pub pair: String,
    pub dir: Dir,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AccountView {
    pub equity: f64,
    /// Equity at the start of the current UTC day.
    pub day_start_equity: f64,
    pub open: Vec<OpenPosition>,
    pub consecutive_losses: usize,
    pub kill_switch: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Proposal {
    pub spec: PairSpec,
    pub dir: Dir,
    pub entry: f64,
    pub stop: f64,
    pub target: f64,
    pub spread_pips: f64,
    /// Unix seconds, used for blackout windows.
    pub now: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    Approve {
        units: f64,
        risk_usd: f64,
        stop_pips: f64,
        reward_risk: f64,
    },
    Reject {
        reason: String,
    },
}

const MICRO_LOT: f64 = 1_000.0;
const SECS_PER_PIP_GUARD: f64 = 1e-12;
/// Absorbs float error so a budget worth exactly 10 micro lots is not sized as 9.
const FLOOR_EPSILON: f64 = 1e-6;

/// USD value of one pip for one unit of the pair at `price`.
pub fn pip_value_usd_per_unit(spec: &PairSpec, price: f64) -> f64 {
    if !price.is_finite() || price <= 0.0 {
        return 0.0;
    }
    if spec.usd_is_base {
        spec.pip / price
    } else {
        spec.pip
    }
}

fn reject(reason: String) -> Decision {
    Decision::Reject { reason }
}

fn finite(p: &Proposal) -> bool {
    [p.entry, p.stop, p.target, p.spread_pips]
        .iter()
        .all(|v| v.is_finite())
}

fn sides_ok(p: &Proposal) -> bool {
    match p.dir {
        Dir::Bull => p.stop < p.entry && p.entry < p.target,
        Dir::Bear => p.target < p.entry && p.entry < p.stop,
    }
}

/// Which side of the dollar a position is on: true means long USD.
fn usd_long(spec: &PairSpec, dir: Dir) -> bool {
    (spec.usd_is_base && dir == Dir::Bull) || (!spec.usd_is_base && dir == Dir::Bear)
}

fn exposure_conflict(acct: &AccountView, p: &Proposal) -> Option<String> {
    let want = usd_long(&p.spec, p.dir);
    for pos in &acct.open {
        let Some(spec) = pair_spec(&pos.pair) else {
            return Some(format!(
                "open position {} is not a supported pair, exposure unknown",
                pos.pair
            ));
        };
        if spec.name == p.spec.name || usd_long(&spec, pos.dir) == want {
            let side = if want { "long" } else { "short" };
            return Some(format!(
                "already {side} USD through {}, one position per USD side",
                pos.pair
            ));
        }
    }
    None
}

pub fn evaluate(cfg: &RiskConfig, acct: &AccountView, p: &Proposal) -> Decision {
    if acct.kill_switch {
        return reject("kill switch is on".into());
    }
    if !finite(p) || !acct.equity.is_finite() || acct.equity <= 0.0 || !sides_ok(p) {
        return reject("invalid proposal: bad numbers or stop and target on the wrong side".into());
    }
    let day_loss = (acct.day_start_equity - acct.equity) / acct.day_start_equity * 100.0;
    if day_loss >= cfg.max_daily_loss_pct {
        return reject(format!(
            "daily loss {day_loss:.2}% reached the {}% limit",
            cfg.max_daily_loss_pct
        ));
    }
    if acct.consecutive_losses >= cfg.max_consecutive_losses {
        return reject(format!(
            "{} losses in a row, entries halted",
            acct.consecutive_losses
        ));
    }
    if acct.open.len() >= cfg.max_open_positions {
        return reject(format!("{} positions open, the maximum", acct.open.len()));
    }
    if cfg.blackouts.iter().any(|(s, e)| p.now >= *s && p.now < *e) {
        return reject("inside a blackout window".into());
    }
    if p.spread_pips > cfg.max_spread_pips {
        return reject(format!(
            "spread {:.1} pips is above the {} pip limit",
            p.spread_pips, cfg.max_spread_pips
        ));
    }
    let stop_pips = (p.entry - p.stop).abs() / p.spec.pip;
    if stop_pips < cfg.min_stop_pips || stop_pips > cfg.max_stop_pips {
        return reject(format!(
            "stop of {stop_pips:.1} pips is outside {} to {}",
            cfg.min_stop_pips, cfg.max_stop_pips
        ));
    }
    let reward_risk = (p.target - p.entry).abs() / (p.entry - p.stop).abs().max(SECS_PER_PIP_GUARD);
    if reward_risk < cfg.min_reward_risk {
        return reject(format!(
            "reward to risk {reward_risk:.2} is below {}",
            cfg.min_reward_risk
        ));
    }
    if let Some(why) = exposure_conflict(acct, p) {
        return reject(why);
    }
    size(cfg, acct, p, stop_pips, reward_risk)
}

fn size(
    cfg: &RiskConfig,
    acct: &AccountView,
    p: &Proposal,
    stop_pips: f64,
    reward_risk: f64,
) -> Decision {
    let pip_value = pip_value_usd_per_unit(&p.spec, p.entry);
    if pip_value <= 0.0 {
        return reject("cannot value a pip at this price".into());
    }
    let wanted = acct.equity * cfg.risk_per_trade_pct / 100.0 / (stop_pips * pip_value);
    let units = ((wanted / MICRO_LOT + FLOOR_EPSILON).floor() * MICRO_LOT).min(cfg.max_units);
    if units < MICRO_LOT {
        return reject(format!(
            "risk budget allows {wanted:.0} units, below one micro lot"
        ));
    }
    Decision::Approve {
        units,
        risk_usd: units * stop_pips * pip_value,
        stop_pips,
        reward_risk,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::pair_spec;

    fn acct() -> AccountView {
        AccountView {
            equity: 10_000.0,
            day_start_equity: 10_000.0,
            open: vec![],
            consecutive_losses: 0,
            kill_switch: false,
        }
    }

    fn long_eurusd() -> Proposal {
        Proposal {
            spec: pair_spec("EURUSD").unwrap(),
            dir: Dir::Bull,
            entry: 1.1000,
            stop: 1.0950,
            target: 1.1100,
            spread_pips: 1.0,
            now: 1_000,
        }
    }

    fn reason(d: Decision) -> String {
        match d {
            Decision::Reject { reason } => reason,
            other => panic!("expected reject, got {other:?}"),
        }
    }

    #[test]
    fn approves_and_sizes_eurusd() {
        // Risk 0.5% of 10,000 = 50 USD over a 50 pip stop at 0.0001 USD per pip per unit = 10,000 units.
        let Decision::Approve {
            units,
            risk_usd,
            stop_pips,
            reward_risk,
        } = evaluate(&RiskConfig::default(), &acct(), &long_eurusd())
        else {
            panic!("expected approve")
        };
        assert_eq!(units, 10_000.0);
        assert!(
            (risk_usd - 50.0).abs() < 1e-9
                && (stop_pips - 50.0).abs() < 1e-6
                && (reward_risk - 2.0).abs() < 1e-9
        );
    }

    #[test]
    fn sizes_usdjpy_with_price_conversion() {
        let p = Proposal {
            spec: pair_spec("USDJPY").unwrap(),
            dir: Dir::Bull,
            entry: 150.0,
            stop: 149.5,
            target: 151.0,
            ..long_eurusd()
        };
        // 50 pips; pip value per unit = 0.01 / 150 USD; 50 USD / (50 * 0.01/150) = 15,000 units.
        let Decision::Approve { units, .. } = evaluate(&RiskConfig::default(), &acct(), &p) else {
            panic!()
        };
        assert_eq!(units, 15_000.0);
    }

    #[test]
    fn units_round_down_and_cap() {
        let mut a = acct();
        a.equity = 10_999.0;
        a.day_start_equity = 10_999.0;
        let Decision::Approve { units, .. } = evaluate(&RiskConfig::default(), &a, &long_eurusd())
        else {
            panic!()
        };
        assert_eq!(units, 10_000.0);
        let cfg = RiskConfig {
            max_units: 5_000.0,
            ..RiskConfig::default()
        };
        let Decision::Approve { units, .. } = evaluate(&cfg, &acct(), &long_eurusd()) else {
            panic!()
        };
        assert_eq!(units, 5_000.0);
    }

    #[test]
    fn each_rule_rejects() {
        let cfg = RiskConfig::default();
        let mut a = acct();
        a.kill_switch = true;
        assert!(reason(evaluate(&cfg, &a, &long_eurusd())).contains("kill switch"));

        let mut bad = long_eurusd();
        bad.target = 1.0900;
        assert!(reason(evaluate(&cfg, &acct(), &bad)).contains("invalid"));

        let mut a = acct();
        a.equity = 9_700.0;
        assert!(reason(evaluate(&cfg, &a, &long_eurusd())).contains("daily loss"));

        let mut a = acct();
        a.consecutive_losses = 4;
        assert!(reason(evaluate(&cfg, &a, &long_eurusd())).contains("in a row"));

        let mut a = acct();
        a.open = vec![
            OpenPosition {
                pair: "GBPUSD".into(),
                dir: Dir::Bear,
            },
            OpenPosition {
                pair: "USDJPY".into(),
                dir: Dir::Bull,
            },
            OpenPosition {
                pair: "AUDUSD".into(),
                dir: Dir::Bear,
            },
        ];
        assert!(reason(evaluate(&cfg, &a, &long_eurusd())).contains("maximum"));

        let cfg_b = RiskConfig {
            blackouts: vec![(900, 1_000)],
            ..RiskConfig::default()
        };
        assert!(
            evaluate(&cfg_b, &acct(), &long_eurusd())
                != Decision::Reject {
                    reason: String::new()
                }
        );
        let mut p = long_eurusd();
        p.spread_pips = 5.0;
        assert!(reason(evaluate(&cfg, &acct(), &p)).contains("spread"));

        let mut p = long_eurusd();
        p.stop = 1.0999;
        assert!(reason(evaluate(&cfg, &acct(), &p)).contains("stop of"));

        let mut p = long_eurusd();
        p.target = 1.1040;
        assert!(reason(evaluate(&cfg, &acct(), &p)).contains("reward to risk"));

        let mut a = acct();
        a.open = vec![OpenPosition {
            pair: "GBPUSD".into(),
            dir: Dir::Bull,
        }];
        assert!(reason(evaluate(&cfg, &a, &long_eurusd())).contains("USD"));
    }

    #[test]
    fn blackout_start_is_inclusive_and_end_exclusive() {
        let cfg = RiskConfig {
            blackouts: vec![(1_000, 2_000)],
            ..RiskConfig::default()
        };
        let mut p = long_eurusd();
        p.now = 1_000;
        assert!(reason(evaluate(&cfg, &acct(), &p)).contains("blackout"));
        p.now = 2_000;
        assert!(matches!(
            evaluate(&cfg, &acct(), &p),
            Decision::Approve { .. }
        ));
    }

    #[test]
    fn tiny_budget_and_nan_are_rejected_without_panic() {
        let cfg = RiskConfig {
            risk_per_trade_pct: 0.01,
            ..RiskConfig::default()
        };
        assert!(reason(evaluate(&cfg, &acct(), &long_eurusd())).contains("micro lot"));
        let mut p = long_eurusd();
        p.entry = f64::NAN;
        assert!(reason(evaluate(&RiskConfig::default(), &acct(), &p)).contains("invalid"));
    }

    #[test]
    fn opposite_usd_side_is_allowed() {
        let mut a = acct();
        a.open = vec![OpenPosition {
            pair: "USDJPY".into(),
            dir: Dir::Bear,
        }];
        // Long EURUSD is short USD, and USDJPY short is also short USD: conflict.
        assert!(reason(evaluate(&RiskConfig::default(), &a, &long_eurusd())).contains("USD"));
        a.open = vec![OpenPosition {
            pair: "USDJPY".into(),
            dir: Dir::Bull,
        }];
        assert!(matches!(
            evaluate(&RiskConfig::default(), &a, &long_eurusd()),
            Decision::Approve { .. }
        ));
    }
}
