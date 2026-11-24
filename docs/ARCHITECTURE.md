# Architecture

```
Yahoo chart endpoint ──> fx-feed ──> SQLite (candles, state, journal)
                                          │
closed candles ──> fx-core detectors ──> strategy ──> risk engine ──> paper broker
                                          │                              │
                                      fx-harness (scheduler, API, chat) <┘
                                          │
                       loopback web app  ←┴→  Android service (alerts, battery, share)
```

## Rules the code keeps
- **Causality.** A detector event exists only from the candle whose close makes it knowable. A test recomputes every prefix of several series and compares. See `docs/smc-definitions.md`.
- **One engine.** The backtester and the live paper loop run the same `Driver`, so fills and risk limits are identical.
- **One account.** Every pair trades one shared paper account, so the exposure rule sees all positions.
- **One timeline.** On each poll the strategy's market view is rebuilt by replaying the stored window, then only newly closed candles can trade. Stored detector indexes would go stale as the window slides.
- **Atomic saves.** Pair state and the account are written in one transaction while the locks that guard them are held.

## Lock order
`pairs`, then `account`, then `risk`, with the store taken last and alone. Code that takes the store must not then take another lock.

## Threat model
One owner, one phone. Other apps and web pages can reach loopback ports. The API needs a custom header carrying a per-install token (so a browser cannot call it cross-site), compares it in constant time, and the token file is private to the app. The Android app uses a random port and a challenge that proves the daemon holds the token before any request carries it. The WebView only loads that origin. The chat can fetch pages and search the web through HQ's guarded client, which refuses private addresses.
