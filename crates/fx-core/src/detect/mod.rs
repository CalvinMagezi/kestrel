//! Causal smart money concept detectors. See `briefs/01-detect.md` for the definitions.
//!
//! Contract: `analyze(candles, cfg)` returns events in non-decreasing `known_at` order, where
//! `known_at` is the index of the candle whose CLOSE first makes the event knowable. For every
//! prefix length `n`, the events with `known_at < n` from `analyze(&candles[..n])` equal exactly
//! the events with `known_at < n` from `analyze(candles)`. A test enforces this.

use crate::types::{Candle, Dir};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq)]
pub struct DetectConfig {
    /// Bars on each side that must be lower (or higher) for a swing point. Confirmed `swing_len`
    /// bars after the swing candle.
    pub swing_len: usize,
    /// A break needs a candle CLOSE beyond the level (true) or any wick beyond it (false).
    pub close_break: bool,
}

impl Default for DetectConfig {
    fn default() -> Self {
        Self {
            swing_len: 5,
            close_break: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub known_at: usize,
    pub kind: EventKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum EventKind {
    /// Swing high (`Bear` side extreme is a high) or low. `idx` is the swing candle.
    Swing { idx: usize, high: bool, price: f64 },
    /// Break of structure (with trend) or change of character (against trend). `broken_at` equals
    /// `known_at`. `swing_idx` is the swing candle whose level broke.
    Break {
        choch: bool,
        dir: Dir,
        level: f64,
        swing_idx: usize,
    },
    /// Fair value gap formed by candles `idx-2`, `idx-1`, `idx`. Zone is `bottom..top`.
    Fvg {
        id: usize,
        dir: Dir,
        top: f64,
        bottom: f64,
    },
    /// Price first traded back into the gap or block `id`.
    Touched { id: usize },
    /// A candle closed fully through the gap or block `id`. It is no longer valid.
    Mitigated { id: usize },
    /// Order block: the origin candle of the move that produced the break at `known_at`.
    OrderBlock {
        id: usize,
        dir: Dir,
        top: f64,
        bottom: f64,
        candle_idx: usize,
    },
    /// Wick beyond a confirmed swing level with a close back inside. `dir` is the side the sweep
    /// expects to reverse towards (`Bull` after a sweep of lows).
    Sweep {
        dir: Dir,
        level: f64,
        swing_idx: usize,
    },
}

const MIN_FVG_SIZE: f64 = 1e-9;
const OB_LOOKBACK: usize = 4;

#[derive(Clone, Copy, Debug)]
struct SwingLevel {
    idx: usize,
    price: f64,
    broken: bool,
    broken_by_close: bool,
    swept: bool,
}

#[derive(Clone, Copy, Debug)]
struct Zone {
    id: usize,
    dir: Dir,
    top: f64,
    bottom: f64,
    formed_at: usize,
    touched: bool,
    mitigated: bool,
}

#[derive(Default)]
struct State {
    last_high: Option<SwingLevel>,
    last_low: Option<SwingLevel>,
    trend: Option<Dir>,
    zones: Vec<Zone>,
    next_id: usize,
}

impl State {
    fn alloc_id(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

/// Detect all events in `candles`.
pub fn analyze(candles: &[Candle], cfg: &DetectConfig) -> Vec<Event> {
    if candles.is_empty() {
        return Vec::new();
    }

    let mut state = State::default();
    let mut events = Vec::new();

    for t in 0..candles.len() {
        if !is_valid_candle(&candles[t]) {
            continue;
        }
        confirm_swings(candles, t, cfg.swing_len, &mut state, &mut events);
        let (bull_qual, bear_qual) =
            evaluate_breaks(candles, t, cfg.close_break, &mut state, &mut events);
        evaluate_sweeps(
            &candles[t],
            t,
            bull_qual,
            bear_qual,
            &mut state,
            &mut events,
        );
        update_zones(&candles[t], t, &mut state, &mut events);
        detect_fvg(candles, t, &mut state, &mut events);
    }

    events
}

fn is_valid_candle(c: &Candle) -> bool {
    c.open.is_finite() && c.high.is_finite() && c.low.is_finite() && c.close.is_finite()
}

fn confirm_swings(
    candles: &[Candle],
    t: usize,
    swing_len: usize,
    state: &mut State,
    events: &mut Vec<Event>,
) {
    let Some(min_bar) = swing_len.checked_mul(2) else {
        return;
    };
    if swing_len == 0 || t < min_bar {
        return;
    }

    let idx = t - swing_len;
    if is_swing_high(candles, idx, swing_len) {
        let price = candles[idx].high;
        events.push(Event {
            known_at: t,
            kind: EventKind::Swing {
                idx,
                high: true,
                price,
            },
        });
        state.last_high = Some(SwingLevel {
            idx,
            price,
            broken: false,
            broken_by_close: false,
            swept: false,
        });
    }
    if is_swing_low(candles, idx, swing_len) {
        let price = candles[idx].low;
        events.push(Event {
            known_at: t,
            kind: EventKind::Swing {
                idx,
                high: false,
                price,
            },
        });
        state.last_low = Some(SwingLevel {
            idx,
            price,
            broken: false,
            broken_by_close: false,
            swept: false,
        });
    }
}

fn is_swing_high(candles: &[Candle], idx: usize, swing_len: usize) -> bool {
    if !is_valid_candle(&candles[idx]) {
        return false;
    }
    let target = candles[idx].high;
    let start = idx - swing_len;
    for (offset, c) in candles[start..=idx + swing_len].iter().enumerate() {
        if start + offset == idx {
            continue;
        }
        if !is_valid_candle(c) || target <= c.high {
            return false;
        }
    }
    true
}

fn is_swing_low(candles: &[Candle], idx: usize, swing_len: usize) -> bool {
    if !is_valid_candle(&candles[idx]) {
        return false;
    }
    let target = candles[idx].low;
    let start = idx - swing_len;
    for (offset, c) in candles[start..=idx + swing_len].iter().enumerate() {
        if start + offset == idx {
            continue;
        }
        if !is_valid_candle(c) || target >= c.low {
            return false;
        }
    }
    true
}

fn breaks_high(c: &Candle, level: f64, close_break: bool) -> bool {
    if close_break {
        c.close > level
    } else {
        c.high > level
    }
}

fn breaks_low(c: &Candle, level: f64, close_break: bool) -> bool {
    if close_break {
        c.close < level
    } else {
        c.low < level
    }
}

fn evaluate_breaks(
    candles: &[Candle],
    t: usize,
    close_break: bool,
    state: &mut State,
    events: &mut Vec<Event>,
) -> (bool, bool) {
    let c = &candles[t];
    let bull_qual = state
        .last_high
        .as_ref()
        .is_some_and(|sh| !sh.broken && breaks_high(c, sh.price, close_break));
    let bear_qual = state
        .last_low
        .as_ref()
        .is_some_and(|sl| !sl.broken && breaks_low(c, sl.price, close_break));

    if bull_qual && bear_qual {
        return (true, true);
    }

    if bull_qual && let Some(sh) = state.last_high.as_mut() {
        sh.broken = true;
        let (level, swing_idx) = (sh.price, sh.idx);
        emit_break_and_ob(candles, t, Dir::Bull, level, swing_idx, state, events);
    } else if bear_qual && let Some(sl) = state.last_low.as_mut() {
        sl.broken = true;
        let (level, swing_idx) = (sl.price, sl.idx);
        emit_break_and_ob(candles, t, Dir::Bear, level, swing_idx, state, events);
    }

    (bull_qual, bear_qual)
}

fn emit_break_and_ob(
    candles: &[Candle],
    t: usize,
    dir: Dir,
    level: f64,
    swing_idx: usize,
    state: &mut State,
    events: &mut Vec<Event>,
) {
    let choch = matches!(
        (state.trend, dir),
        (Some(Dir::Bear), Dir::Bull) | (Some(Dir::Bull), Dir::Bear)
    );
    state.trend = Some(dir);
    events.push(Event {
        known_at: t,
        kind: EventKind::Break {
            choch,
            dir,
            level,
            swing_idx,
        },
    });

    let origin = find_leg_origin(candles, swing_idx, t, dir);
    let candle_idx = find_ob_candle(candles, origin, dir);
    let ob = &candles[candle_idx];
    let id = state.alloc_id();

    events.push(Event {
        known_at: t,
        kind: EventKind::OrderBlock {
            id,
            dir,
            top: ob.high,
            bottom: ob.low,
            candle_idx,
        },
    });
    state.zones.push(Zone {
        id,
        dir,
        top: ob.high,
        bottom: ob.low,
        formed_at: t,
        touched: false,
        mitigated: false,
    });
}

fn find_leg_origin(candles: &[Candle], swing_idx: usize, t: usize, dir: Dir) -> usize {
    let mut best: Option<(usize, f64)> = None;
    let start = swing_idx + 1;
    for (offset, c) in candles[start..=t].iter().enumerate() {
        if !is_valid_candle(c) {
            continue;
        }
        let val = match dir {
            Dir::Bull => c.low,
            Dir::Bear => c.high,
        };
        let better = match best {
            None => true,
            Some((_, prev)) => match dir {
                Dir::Bull => val < prev,
                Dir::Bear => val > prev,
            },
        };
        if better {
            best = Some((start + offset, val));
        }
    }
    best.map_or(t, |(idx, _)| idx)
}

fn find_ob_candle(candles: &[Candle], origin: usize, dir: Dir) -> usize {
    let start = origin.saturating_sub(OB_LOOKBACK);
    for k in (start..=origin).rev() {
        if !is_valid_candle(&candles[k]) {
            continue;
        }
        let is_opposing = match dir {
            Dir::Bull => candles[k].close < candles[k].open,
            Dir::Bear => candles[k].close > candles[k].open,
        };
        if is_opposing {
            return k;
        }
    }
    origin
}

fn evaluate_sweeps(
    c: &Candle,
    t: usize,
    bull_qual: bool,
    bear_qual: bool,
    state: &mut State,
    events: &mut Vec<Event>,
) {
    if let Some(sh) = state.last_high.as_mut() {
        if c.close > sh.price {
            sh.broken_by_close = true;
        } else if !bull_qual && !sh.broken_by_close && !sh.swept && c.high > sh.price {
            sh.swept = true;
            events.push(Event {
                known_at: t,
                kind: EventKind::Sweep {
                    dir: Dir::Bear,
                    level: sh.price,
                    swing_idx: sh.idx,
                },
            });
        }
    }

    if let Some(sl) = state.last_low.as_mut() {
        if c.close < sl.price {
            sl.broken_by_close = true;
        } else if !bear_qual && !sl.broken_by_close && !sl.swept && c.low < sl.price {
            sl.swept = true;
            events.push(Event {
                known_at: t,
                kind: EventKind::Sweep {
                    dir: Dir::Bull,
                    level: sl.price,
                    swing_idx: sl.idx,
                },
            });
        }
    }
}

fn update_zones(c: &Candle, t: usize, state: &mut State, events: &mut Vec<Event>) {
    for zone in &mut state.zones {
        if zone.formed_at >= t {
            continue;
        }
        if !zone.touched && c.low <= zone.top && c.high >= zone.bottom {
            zone.touched = true;
            events.push(Event {
                known_at: t,
                kind: EventKind::Touched { id: zone.id },
            });
        }
        let mitigated = match zone.dir {
            Dir::Bull => c.close < zone.bottom,
            Dir::Bear => c.close > zone.top,
        };
        if mitigated {
            zone.mitigated = true;
            events.push(Event {
                known_at: t,
                kind: EventKind::Mitigated { id: zone.id },
            });
        }
    }
    state.zones.retain(|z| !z.mitigated);
}

fn detect_fvg(candles: &[Candle], t: usize, state: &mut State, events: &mut Vec<Event>) {
    if t < 2 || !is_valid_candle(&candles[t - 2]) || !is_valid_candle(&candles[t - 1]) {
        return;
    }

    let c = &candles[t];
    let prev2 = &candles[t - 2];
    let bull_gap = c.low - prev2.high;
    let bear_gap = prev2.low - c.high;

    let zone_spec = if bull_gap >= MIN_FVG_SIZE {
        Some((Dir::Bull, c.low, prev2.high))
    } else if bear_gap >= MIN_FVG_SIZE {
        Some((Dir::Bear, prev2.low, c.high))
    } else {
        None
    };

    let Some((dir, top, bottom)) = zone_spec else {
        return;
    };

    let id = state.alloc_id();
    events.push(Event {
        known_at: t,
        kind: EventKind::Fvg {
            id,
            dir,
            top,
            bottom,
        },
    });
    state.zones.push(Zone {
        id,
        dir,
        top,
        bottom,
        formed_at: t,
        touched: false,
        mitigated: false,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(i: usize, open: f64, high: f64, low: f64, close: f64) -> Candle {
        Candle {
            ts: (i as i64) * 3600,
            open,
            high,
            low,
            close,
        }
    }

    fn xorshift64(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    fn rand_unit(state: &mut u64) -> f64 {
        (xorshift64(state) >> 11) as f64 / ((1u64 << 53) as f64)
    }

    fn gen_random_candles(seed: u64, count: usize) -> Vec<Candle> {
        let mut rng = seed;
        let mut price = 1.1000;
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let gap = (rand_unit(&mut rng) - 0.5) * 0.0040;
            let open = price + gap;
            let body = (rand_unit(&mut rng) - 0.5) * 0.0080;
            let close = open + body;
            let upper = rand_unit(&mut rng) * 0.0050;
            let lower = rand_unit(&mut rng) * 0.0050;
            let high = open.max(close) + upper;
            let low = open.min(close) - lower;
            out.push(mk(i, open, high, low, close));
            price = close;
        }
        out
    }

    fn gen_trending_candles(count: usize) -> Vec<Candle> {
        let mut out = Vec::with_capacity(count);
        let mut base: f64 = 1.0000;
        for i in 0..count {
            let phase = i % 10;
            let delta = match phase {
                0..=5 => 0.0030,
                6..=7 => -0.0040,
                _ => 0.0025,
            };
            let open = base;
            let close = base + delta;
            let high = open.max(close) + 0.0015;
            let low = open.min(close) - 0.0015;
            out.push(mk(i, open, high, low, close));
            base = close;
        }
        out
    }

    fn gen_ranging_candles(count: usize) -> Vec<Candle> {
        let mut out = Vec::with_capacity(count);
        let levels: [f64; 10] = [
            1.100, 1.104, 1.108, 1.112, 1.108, 1.104, 1.100, 1.096, 1.100, 1.104,
        ];
        for i in 0..count {
            let open = levels[i % levels.len()];
            let close = levels[(i + 1) % levels.len()];
            let wick = if i % 7 == 0 { 0.004 } else { 0.001 };
            let high = open.max(close) + wick;
            let low = open.min(close) - wick;
            out.push(mk(i, open, high, low, close));
        }
        out
    }

    fn assert_causal_truncation(candles: &[Candle], cfg: &DetectConfig) {
        let full = analyze(candles, cfg);
        for w in full.windows(2) {
            assert!(
                w[0].known_at <= w[1].known_at,
                "events must be sorted by known_at"
            );
        }
        for n in 1..=candles.len() {
            let prefix = analyze(&candles[..n], cfg);
            let expected: Vec<Event> = full.iter().filter(|e| e.known_at < n).cloned().collect();
            assert_eq!(prefix, expected, "truncation mismatch at prefix length {n}");
        }
    }

    fn assert_all_event_kinds_present(events: &[Event]) {
        assert!(
            events
                .iter()
                .any(|e| matches!(e.kind, EventKind::Swing { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e.kind, EventKind::Break { choch: false, .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e.kind, EventKind::Break { choch: true, .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e.kind, EventKind::Fvg { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e.kind, EventKind::Touched { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e.kind, EventKind::Mitigated { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e.kind, EventKind::OrderBlock { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e.kind, EventKind::Sweep { .. }))
        );
    }

    #[test]
    fn truncation_invariant_holds_on_random_trending_and_ranging_series() {
        let seeds = [
            0x1234_5678_9abc_def0,
            0xdead_beef_cafe_babe,
            0x0bad_f00d_1337_c0de,
        ];
        let cfgs = [
            DetectConfig {
                swing_len: 3,
                close_break: true,
            },
            DetectConfig {
                swing_len: 5,
                close_break: false,
            },
        ];

        for &seed in &seeds {
            let candles = gen_random_candles(seed, 250);
            for cfg in &cfgs {
                assert_causal_truncation(&candles, cfg);
            }
            assert_all_event_kinds_present(&analyze(&candles, &cfgs[0]));
        }

        let trending = gen_trending_candles(120);
        let ranging = gen_ranging_candles(120);
        let cfg = DetectConfig {
            swing_len: 3,
            close_break: true,
        };
        assert_causal_truncation(&trending, &cfg);
        assert_causal_truncation(&ranging, &cfg);
    }

    #[test]
    fn swing_known_at_equals_idx_plus_swing_len() {
        let cfg = DetectConfig {
            swing_len: 2,
            close_break: true,
        };
        let candles = vec![
            mk(0, 1.00, 1.02, 0.99, 1.01),
            mk(1, 1.01, 1.05, 1.00, 1.04),
            mk(2, 1.04, 1.10, 1.03, 1.08),
            mk(3, 1.08, 1.06, 1.02, 1.03),
            mk(4, 1.03, 1.04, 1.01, 1.02),
            mk(5, 1.02, 1.03, 0.95, 0.96),
            mk(6, 0.96, 1.00, 0.97, 0.99),
            mk(7, 0.99, 1.02, 0.98, 1.01),
        ];

        let events = analyze(&candles, &cfg);
        let swings: Vec<(usize, usize, bool, f64)> = events
            .iter()
            .filter_map(|e| match e.kind {
                EventKind::Swing { idx, high, price } => Some((e.known_at, idx, high, price)),
                _ => None,
            })
            .collect();

        assert_eq!(swings, vec![(4, 2, true, 1.10), (7, 5, false, 0.95)]);
        for (known_at, idx, _, _) in swings {
            assert_eq!(known_at, idx + cfg.swing_len);
        }
    }

    #[test]
    fn bos_then_choch_ordering_and_levels() {
        let cfg = DetectConfig {
            swing_len: 1,
            close_break: true,
        };
        let candles = vec![
            mk(0, 1.00, 1.02, 0.99, 1.01),
            mk(1, 1.01, 1.06, 1.00, 1.05),
            mk(2, 1.05, 1.04, 1.01, 1.02),
            mk(3, 1.02, 1.03, 0.98, 0.99),
            mk(4, 0.99, 1.03, 0.99, 1.02),
            mk(5, 1.02, 1.08, 1.01, 1.07),
            mk(6, 1.07, 1.07, 0.96, 0.97),
        ];

        let breaks: Vec<(usize, bool, Dir, f64, usize)> = analyze(&candles, &cfg)
            .into_iter()
            .filter_map(|e| match e.kind {
                EventKind::Break {
                    choch,
                    dir,
                    level,
                    swing_idx,
                } => Some((e.known_at, choch, dir, level, swing_idx)),
                _ => None,
            })
            .collect();

        assert_eq!(
            breaks,
            vec![
                (5, false, Dir::Bull, 1.06, 1),
                (6, true, Dir::Bear, 0.98, 3),
            ]
        );
    }

    #[test]
    fn wick_through_swing_high_closing_inside_yields_sweep_and_no_break() {
        let cfg = DetectConfig {
            swing_len: 1,
            close_break: true,
        };
        let candles = vec![
            mk(0, 1.00, 1.02, 0.99, 1.01),
            mk(1, 1.01, 1.05, 1.00, 1.04),
            mk(2, 1.04, 1.03, 1.01, 1.02),
            mk(3, 1.02, 1.07, 1.01, 1.04),
            mk(4, 1.04, 1.08, 1.02, 1.03),
        ];

        let events = analyze(&candles, &cfg);
        let breaks: Vec<_> = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Break { .. }))
            .collect();
        let sweeps: Vec<_> = events
            .iter()
            .filter_map(|e| match e.kind {
                EventKind::Sweep {
                    dir,
                    level,
                    swing_idx,
                } => Some((e.known_at, dir, level, swing_idx)),
                _ => None,
            })
            .collect();

        assert!(breaks.is_empty());
        assert_eq!(sweeps, vec![(3, Dir::Bear, 1.05, 1)]);
    }

    #[test]
    fn close_break_false_breaks_on_wick_where_true_does_not() {
        let candles = vec![
            mk(0, 1.00, 1.02, 0.99, 1.01),
            mk(1, 1.01, 1.05, 1.00, 1.04),
            mk(2, 1.04, 1.03, 1.01, 1.02),
            mk(3, 1.02, 1.07, 1.01, 1.04),
        ];

        let with_close = analyze(
            &candles,
            &DetectConfig {
                swing_len: 1,
                close_break: true,
            },
        );
        let with_wick = analyze(
            &candles,
            &DetectConfig {
                swing_len: 1,
                close_break: false,
            },
        );

        assert!(
            !with_close
                .iter()
                .any(|e| matches!(e.kind, EventKind::Break { .. }))
        );
        assert!(
            with_close
                .iter()
                .any(|e| matches!(e.kind, EventKind::Sweep { .. }))
        );

        assert!(with_wick.iter().any(|e| matches!(
            e.kind,
            EventKind::Break { choch: false, dir: Dir::Bull, level, swing_idx: 1 } if level == 1.05
        )));
        assert!(
            !with_wick
                .iter()
                .any(|e| matches!(e.kind, EventKind::Sweep { .. }))
        );
    }

    #[test]
    fn bullish_and_bearish_fvg_lifecycle_touched_then_mitigated() {
        let cfg = DetectConfig {
            swing_len: 10,
            close_break: true,
        };
        let bull_candles = vec![
            mk(0, 1.00, 1.02, 0.99, 1.01),
            mk(1, 1.01, 1.06, 1.01, 1.05),
            mk(2, 1.05, 1.08, 1.04, 1.07),
            mk(3, 1.07, 1.08, 1.03, 1.06),
            mk(4, 1.06, 1.06, 1.01, 1.01),
            mk(5, 1.01, 1.03, 1.00, 1.00),
        ];

        let bull_events = analyze(&bull_candles, &cfg);
        assert_eq!(
            bull_events,
            vec![
                Event {
                    known_at: 2,
                    kind: EventKind::Fvg {
                        id: 0,
                        dir: Dir::Bull,
                        top: 1.04,
                        bottom: 1.02
                    },
                },
                Event {
                    known_at: 3,
                    kind: EventKind::Touched { id: 0 }
                },
                Event {
                    known_at: 4,
                    kind: EventKind::Mitigated { id: 0 }
                },
            ]
        );

        let bear_candles = vec![
            mk(0, 1.10, 1.11, 1.08, 1.09),
            mk(1, 1.09, 1.09, 1.04, 1.05),
            mk(2, 1.05, 1.06, 1.02, 1.03),
            mk(3, 1.03, 1.10, 1.03, 1.09),
            mk(4, 1.09, 1.11, 1.05, 1.10),
        ];

        let bear_events = analyze(&bear_candles, &cfg);
        assert_eq!(
            bear_events,
            vec![
                Event {
                    known_at: 2,
                    kind: EventKind::Fvg {
                        id: 0,
                        dir: Dir::Bear,
                        top: 1.08,
                        bottom: 1.06
                    },
                },
                Event {
                    known_at: 3,
                    kind: EventKind::Touched { id: 0 }
                },
                Event {
                    known_at: 3,
                    kind: EventKind::Mitigated { id: 0 }
                },
            ]
        );
    }

    #[test]
    fn order_block_picks_last_bearish_candle_before_origin() {
        let cfg = DetectConfig {
            swing_len: 1,
            close_break: true,
        };
        let candles = vec![
            mk(0, 1.00, 1.02, 0.99, 1.01),
            mk(1, 1.01, 1.10, 1.00, 1.09),
            mk(2, 1.09, 1.09, 1.05, 1.06),
            mk(3, 1.06, 1.07, 1.02, 1.03),
            mk(4, 1.03, 1.04, 1.00, 1.04),
            mk(5, 1.04, 1.12, 1.03, 1.11),
        ];

        let events = analyze(&candles, &cfg);
        let obs: Vec<_> = events
            .iter()
            .filter_map(|e| match e.kind {
                EventKind::OrderBlock {
                    dir,
                    top,
                    bottom,
                    candle_idx,
                    ..
                } => Some((e.known_at, dir, top, bottom, candle_idx)),
                _ => None,
            })
            .collect();

        assert_eq!(obs, vec![(5, Dir::Bull, 1.07, 1.02, 3)]);
    }

    #[test]
    fn edge_cases_short_flat_empty_and_nan_do_not_panic() {
        let cfg = DetectConfig {
            swing_len: 3,
            close_break: true,
        };

        assert!(analyze(&[], &cfg).is_empty());

        let short: Vec<Candle> = (0..2 * cfg.swing_len)
            .map(|i| {
                let peak = if i == cfg.swing_len { 1.05 } else { 1.01 };
                mk(i, 1.00, peak, 0.99, 1.00)
            })
            .collect();
        assert!(analyze(&short, &cfg).is_empty());

        let flat: Vec<Candle> = (0..30).map(|i| mk(i, 1.10, 1.10, 1.10, 1.10)).collect();
        assert!(analyze(&flat, &cfg).is_empty());

        let mut with_nan = gen_random_candles(42, 40);
        with_nan[5].high = f64::NAN;
        with_nan[12].close = f64::INFINITY;
        with_nan[20].low = f64::NEG_INFINITY;
        assert_causal_truncation(
            &with_nan,
            &DetectConfig {
                swing_len: 2,
                close_break: true,
            },
        );
    }

    #[test]
    fn simultaneous_bull_and_bear_break_emits_neither_and_leaves_levels_unbroken() {
        let cfg = DetectConfig {
            swing_len: 1,
            close_break: false,
        };
        let candles = vec![
            mk(0, 1.00, 1.02, 0.99, 1.01),
            mk(1, 1.01, 1.05, 1.00, 1.04),
            mk(2, 1.04, 1.03, 1.00, 1.01),
            mk(3, 1.01, 1.02, 0.98, 0.99),
            mk(4, 0.99, 1.02, 0.99, 1.01),
            mk(5, 1.01, 1.07, 0.96, 1.02),
            mk(6, 1.02, 1.07, 1.01, 1.06),
        ];

        let breaks: Vec<_> = analyze(&candles, &cfg)
            .into_iter()
            .filter_map(|e| match e.kind {
                EventKind::Break {
                    dir,
                    level,
                    swing_idx,
                    ..
                } => Some((e.known_at, dir, level, swing_idx)),
                _ => None,
            })
            .collect();

        assert_eq!(breaks, vec![(6, Dir::Bull, 1.05, 1)]);
    }

    #[test]
    fn tiny_fvg_below_threshold_is_ignored() {
        let cfg = DetectConfig {
            swing_len: 5,
            close_break: true,
        };
        let candles = vec![
            mk(0, 1.00, 1.02, 0.99, 1.01),
            mk(1, 1.01, 1.03, 1.00, 1.02),
            mk(2, 1.02 + 5e-10, 1.04, 1.02 + 5e-10, 1.03),
        ];
        assert!(analyze(&candles, &cfg).is_empty());
    }
}
