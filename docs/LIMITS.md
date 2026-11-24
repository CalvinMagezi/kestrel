# Limits

Read this before trusting any number Kestrel shows.

## What the results mean
- The strategy that ships with Kestrel showed no edge over random entries on the data it was tested on (EURUSD, hourly, about 3,000 bars, modelled costs). That is one pair, one window, one set of parameters. It is a measurement, not a conclusion about smart money concepts.
- A backtest uses mid prices and modelled spread, slippage and swap. Real brokers differ, and the swap table is illustrative.
- A few hundred trades are needed before a small edge becomes visible. A forward test of a few weeks is a start, not proof.

## The data
- Candles come from Yahoo's public chart endpoint. It is unofficial, has no service guarantee, can change or throttle without notice, and its terms are the user's responsibility. Kestrel caches what it downloads and keeps working from the cache.
- The four-hour candle is built from hourly candles. A late final hourly bar is handled with a ten-minute grace window, not perfectly.
- FX volume is not available and is not used.

## The journal
- It is tamper-evident, not tamper-proof. Editing or deleting an entry in the middle breaks the chain. Deleting the newest entries cannot be detected unless the latest seal was noted elsewhere. Someone who controls the device can rebuild the whole chain from scratch.
- Old entries are pruned after 20,000 rows. The chain stays valid from the oldest row kept.

## The app
- The daemon survives the screen closing as a foreground service. Phone makers add their own battery rules (Samsung's sleeping apps, for example) that the app can only point you to.
- Sharing very large exports is capped at the newest 200,000 characters. The full journal is available from the local API.
- One timeframe at a time, three majors by default, USD-based or USD-quoted pairs only.
- Alerts are checked every 30 seconds and are only as timely as the candles: setups form when an hourly candle closes.
