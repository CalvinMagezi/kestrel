//! Yahoo Finance chart endpoint. Unofficial and keyless: it can change or rate limit without
//! notice, which is why candles are cached and the harness keeps working from the cache.

use anyhow::{Context, Result, bail};
use fx_core::{Candle, Timeframe, pair_spec};
use serde_json::Value;

const BASE: &str = "https://query1.finance.yahoo.com/v8/finance/chart/";
const LATE_BAR_GRACE_SECS: i64 = 600;
const USER_AGENT: &str = "Mozilla/5.0 (Linux; Android 14) fx-harness";

/// `full` asks for all the history Yahoo keeps; otherwise only the last few days, which is all a
/// regular poll needs and costs a few kilobytes instead of over a megabyte.
fn interval_and_range(tf: Timeframe, full: bool) -> (&'static str, &'static str) {
    match (tf, full) {
        (Timeframe::M15, true) => ("15m", "60d"),
        (Timeframe::M15, false) => ("15m", "5d"),
        (Timeframe::H1 | Timeframe::H4, true) => ("60m", "730d"),
        (Timeframe::H1 | Timeframe::H4, false) => ("60m", "5d"),
        (Timeframe::D1, true) => ("1d", "10y"),
        (Timeframe::D1, false) => ("1d", "1mo"),
    }
}

/// Fetches closed candles for `pair`, with the bytes the reply cost. `now` (unix seconds) decides
/// which candle is still forming.
pub async fn fetch(
    client: &reqwest::Client,
    pair: &str,
    tf: Timeframe,
    now: i64,
    full: bool,
) -> Result<(Vec<Candle>, usize)> {
    let spec = pair_spec(pair).with_context(|| format!("unsupported pair {pair}"))?;
    let (interval, range) = interval_and_range(tf, full);
    let (body, bytes) = get_json(
        client,
        &format!("{BASE}{}=X?interval={interval}&range={range}", spec.name),
    )
    .await?;
    let raw = parse(&body)?;
    let base_tf = if tf == Timeframe::H4 {
        Timeframe::H1
    } else {
        tf
    };
    let closed = drop_unfinished(raw, base_tf, now);
    let out = if tf == Timeframe::H4 {
        aggregate(&closed, tf.secs(), now)
    } else {
        closed
    };
    Ok((out, bytes))
}

/// The latest daily close of a Yahoo symbol such as `UGX=X`, with the bytes it cost.
pub async fn spot(client: &reqwest::Client, symbol: &str) -> Result<(f64, usize)> {
    let (body, bytes) = get_json(client, &format!("{BASE}{symbol}?interval=1d&range=5d")).await?;
    let last = parse_close(&body)?;
    Ok((last, bytes))
}

async fn get_json(client: &reqwest::Client, url: &str) -> Result<(Value, usize)> {
    let resp = client
        .get(url)
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .context("feed request failed")?
        .error_for_status()
        .context("feed returned an error status")?;
    let bytes = resp.bytes().await.context("feed reply was cut off")?;
    let body = serde_json::from_slice(&bytes).context("feed returned unreadable JSON")?;
    Ok((body, bytes.len()))
}

/// The last non-null close in a chart response, for symbols whose candles are not used.
pub fn parse_close(body: &Value) -> Result<f64> {
    let result = &body["chart"]["result"][0];
    if result.is_null() {
        bail!("feed has no data");
    }
    result["indicators"]["quote"][0]["close"]
        .as_array()
        .and_then(|c| c.iter().rev().find_map(Value::as_f64))
        .filter(|v| *v > 0.0)
        .context("feed has no close price")
}

pub fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(25))
        .build()?)
}

/// Parses a Yahoo chart response. Rows with null or impossible prices are dropped.
pub fn parse(body: &Value) -> Result<Vec<Candle>> {
    let result = &body["chart"]["result"][0];
    if result.is_null() {
        let why = body["chart"]["error"]["description"]
            .as_str()
            .unwrap_or("no result");
        bail!("feed has no data: {why}");
    }
    let ts = result["timestamp"]
        .as_array()
        .context("feed response has no timestamps")?;
    let q = &result["indicators"]["quote"][0];
    let col = |name: &str| q[name].as_array().cloned().unwrap_or_default();
    let (o, h, l, c) = (col("open"), col("high"), col("low"), col("close"));
    let mut out = Vec::with_capacity(ts.len());
    for (i, t) in ts.iter().enumerate() {
        let vals = [&o, &h, &l, &c].map(|v| v.get(i).and_then(Value::as_f64));
        let (Some(t), [Some(open), Some(high), Some(low), Some(close)]) = (t.as_i64(), vals) else {
            continue;
        };
        let sane = low > 0.0 && high >= low && high >= open.max(close) && low <= open.min(close);
        if sane {
            out.push(Candle {
                ts: t,
                open,
                high,
                low,
                close,
            });
        }
    }
    out.sort_by_key(|c| c.ts);
    out.dedup_by_key(|c| c.ts);
    Ok(out)
}

/// A candle is closed once its whole period has passed.
pub fn drop_unfinished(mut candles: Vec<Candle>, tf: Timeframe, now: i64) -> Vec<Candle> {
    candles.retain(|c| c.ts + tf.secs() <= now);
    candles
}

/// Builds `secs`-long candles from smaller ones, aligned to multiples of `secs` since the epoch.
/// A bucket is emitted only when it is complete (its last hour has closed).
pub fn aggregate(small: &[Candle], secs: i64, now: i64) -> Vec<Candle> {
    let mut out: Vec<Candle> = Vec::new();
    for c in small {
        let bucket = c.ts.div_euclid(secs) * secs;
        match out.last_mut() {
            Some(b) if b.ts == bucket => {
                b.high = b.high.max(c.high);
                b.low = b.low.min(c.low);
                b.close = c.close;
            }
            _ => out.push(Candle { ts: bucket, ..*c }),
        }
    }
    // A late final hourly bar would leave the last bucket short and it would never be corrected.
    out.retain(|b| b.ts + secs + LATE_BAR_GRACE_SECS <= now);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body() -> Value {
        json!({"chart":{"result":[{"timestamp":[1000,1900,2800,3700,4600],
          "indicators":{"quote":[{
            "open":[1.10,1.11,null,1.12,1.13],
            "high":[1.12,1.13,null,1.14,1.15],
            "low":[1.09,1.10,null,1.11,1.12],
            "close":[1.11,1.12,null,1.13,1.14]}]}}],"error":null}})
    }

    #[test]
    fn spot_takes_the_last_real_close() {
        let b = json!({"chart":{"result":[{"timestamp":[1,2,3],"indicators":{"quote":[{"close":[3600.5, 3700.25, null]}]}}]}});
        assert_eq!(parse_close(&b).unwrap(), 3700.25);
        assert!(parse_close(&json!({"chart":{"result":null}})).is_err());
    }

    #[test]
    fn regular_polls_ask_for_days_not_years() {
        assert_eq!(interval_and_range(Timeframe::H1, true), ("60m", "730d"));
        assert_eq!(interval_and_range(Timeframe::H1, false), ("60m", "5d"));
        assert_eq!(interval_and_range(Timeframe::H4, false), ("60m", "5d"));
        assert_eq!(interval_and_range(Timeframe::D1, false), ("1d", "1mo"));
    }

    #[test]
    fn null_rows_are_dropped() {
        let c = parse(&body()).unwrap();
        assert_eq!(c.len(), 4);
        assert_eq!(c[2].ts, 3700);
    }

    #[test]
    fn impossible_prices_are_dropped() {
        let mut b = body();
        b["chart"]["result"][0]["indicators"]["quote"][0]["high"][0] = json!(1.00);
        assert_eq!(parse(&b).unwrap().len(), 3);
    }

    #[test]
    fn error_response_is_reported() {
        let b = json!({"chart":{"result":null,"error":{"description":"Not Found"}}});
        assert!(parse(&b).unwrap_err().to_string().contains("Not Found"));
    }

    #[test]
    fn forming_candle_is_dropped() {
        let c = parse(&body()).unwrap();
        let closed = drop_unfinished(c, Timeframe::M15, 3700 + 899);
        assert_eq!(closed.last().unwrap().ts, 1900);
    }

    #[test]
    fn four_hour_buckets_aggregate_and_skip_incomplete() {
        let h = 3_600;
        let hours: Vec<Candle> = (0..6)
            .map(|i| Candle {
                ts: i * h,
                open: 1.0 + i as f64,
                high: 2.0 + i as f64,
                low: 0.5 + i as f64,
                close: 1.5 + i as f64,
            })
            .collect();
        let out = aggregate(&hours, 4 * h, 5 * h);
        assert_eq!(out.len(), 1);
        assert_eq!(
            (out[0].open, out[0].high, out[0].low, out[0].close),
            (1.0, 5.0, 0.5, 4.5)
        );
    }
}
