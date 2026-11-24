//! Pure trading logic: no network, no clock, no filesystem. Every function is deterministic so the
//! same candles give the same signals on a laptop and on a phone.

pub mod backtest;
pub mod detect;
pub mod risk;
pub mod sim;
pub mod stats;
pub mod strategy;
pub mod types;

pub use types::{Candle, Dir, PairSpec, Timeframe, pair_spec};
