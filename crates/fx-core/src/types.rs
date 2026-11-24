use serde::{Deserialize, Serialize};

/// One closed candle. `ts` is the open time in unix seconds UTC. FX volume is not carried.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Candle {
    pub ts: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Timeframe {
    M15,
    H1,
    H4,
    D1,
}

impl Timeframe {
    pub fn secs(self) -> i64 {
        match self {
            Timeframe::M15 => 900,
            Timeframe::H1 => 3_600,
            Timeframe::H4 => 14_400,
            Timeframe::D1 => 86_400,
        }
    }
}

/// Direction of a structure break, gap, or block. `Bull` is up, `Bear` is down.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dir {
    Bull,
    Bear,
}

impl Dir {
    pub fn sign(self) -> f64 {
        match self {
            Dir::Bull => 1.0,
            Dir::Bear => -1.0,
        }
    }
}

/// A supported pair. v1 supports only USD-quoted and USD-based majors so pip value converts to USD
/// with one price and no cross rates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PairSpec {
    pub name: &'static str,
    pub pip: f64,
    pub usd_is_base: bool,
    pub default_spread_pips: f64,
}

pub const PAIRS: [PairSpec; 7] = [
    PairSpec {
        name: "EURUSD",
        pip: 0.0001,
        usd_is_base: false,
        default_spread_pips: 1.0,
    },
    PairSpec {
        name: "GBPUSD",
        pip: 0.0001,
        usd_is_base: false,
        default_spread_pips: 1.2,
    },
    PairSpec {
        name: "AUDUSD",
        pip: 0.0001,
        usd_is_base: false,
        default_spread_pips: 1.2,
    },
    PairSpec {
        name: "NZDUSD",
        pip: 0.0001,
        usd_is_base: false,
        default_spread_pips: 1.5,
    },
    PairSpec {
        name: "USDJPY",
        pip: 0.01,
        usd_is_base: true,
        default_spread_pips: 1.0,
    },
    PairSpec {
        name: "USDCHF",
        pip: 0.0001,
        usd_is_base: true,
        default_spread_pips: 1.3,
    },
    PairSpec {
        name: "USDCAD",
        pip: 0.0001,
        usd_is_base: true,
        default_spread_pips: 1.5,
    },
];

/// Accepts `EURUSD`, `EUR/USD`, `eurusd` and `EURUSD=X`.
pub fn pair_spec(name: &str) -> Option<PairSpec> {
    let key: String = name
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .take(6)
        .collect::<String>()
        .to_ascii_uppercase();
    PAIRS.iter().copied().find(|p| p.name == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_names_normalise() {
        assert_eq!(pair_spec("eur/usd").unwrap().name, "EURUSD");
        assert_eq!(pair_spec("USDJPY=X").unwrap().pip, 0.01);
        assert!(pair_spec("EURGBP").is_none());
    }
}
