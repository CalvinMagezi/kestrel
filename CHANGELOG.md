# Changelog

Each release has a section here. The release pipeline publishes the section for the tag as the release notes, and the app shows it under "What's new" when an update is offered.

## 0.1.0

First public release.

- Runs on your phone as a background service and watches hourly candles for EURUSD, GBPUSD and USDJPY.
- Tries a fixed set of smart money concept setups on a paper account with modelled spread, slippage and swap. It never places a real order.
- Every backtest is shown next to a random-entry baseline and a 95% interval.
- A hash-chained journal that shows tampering, with a verify command and sharing.
- Local alerts for setups and trades, silent at night.
- Shows balances in shillings, a trade-size calculator, and a Learn tab with plain explanations and scam red flags.
- Shows how much market data it has used.
- Checks for updates twice a day and installs them after you confirm.
