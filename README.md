# Kestrel

A forex research instrument that runs on your own phone, around the clock, and tells you the truth about a trading idea before it costs you money.

Kestrel never places a real order and is linked to no broker. It watches closed candles on a few major pairs, looks for a fixed set of smart money concept setups, trades them on a paper account with realistic costs, and keeps a journal that cannot be edited without it showing.

## What makes it different

- **It runs where you are.** A small daemon runs as a foreground service on the phone you already own. No server, no account, no subscription. Market data comes from a keyless public source and the app keeps working from its cache when the network drops.
- **It does not flatter you.** Every backtest is shown next to a random-entry baseline with the same stops and costs, and a 95% interval on the result. The detectors are tested so they cannot see the future.
- **It keeps receipts.** Journal entries are hash-chained: change or remove an old entry and the check fails. Note the latest seal somewhere you trust and you can later show the record was not rewritten.
- **It is light on data.** It shows how much market data it has used today and this month.
- **It is honest about its limits.** The first strategy it ships with shows no edge over random entries. See `docs/LIMITS.md`.

Kestrel is research software. Nothing in it is financial advice.

## Layout

| Path | What |
|---|---|
| `crates/fx-core` | Pure logic: detectors, risk engine, paper broker, backtester, statistics. No network, no clock. |
| `crates/fx-feed` | Keyless market data (Yahoo chart endpoint) and the SQLite store, including the hash-chained journal. |
| `crates/fx-harness` | The `fxh` daemon: scheduler, chat, loopback web app and JSON API. |
| `android/` | The Android shell: foreground service, WebView, notifications, a small settings bridge. |
| `docs/` | Architecture, limits, and the written definitions of every setup. |
| `scripts/ui-check.mjs` | Drives the app in headless Chrome and checks every screen in both themes. |

## Run it on a computer

```sh
cargo run -p fx-harness -- --data-dir ./fx-data --port 8787
# the first run writes ./fx-data/token; open the app with it:
open "http://127.0.0.1:8787/?t=$(cat ./fx-data/token)"
```

Free-text chat uses a model only if you give it a key (`anthropic.key` in the data folder or `FX_ANTHROPIC_KEY`). Without one, every command still works.

## Build the Android app

1. Install the Android NDK (28.x) and add the target: `rustup target add aarch64-linux-android`.
2. Build the daemon at API 28 with the NDK clang as linker and `CC_aarch64_linux_android`, `AR_aarch64_linux_android`, `ANDROID_NDK_HOME` set: `cargo build --release --target aarch64-linux-android -p fx-harness`.
3. `cd android && ./gradlew :app:assembleDebug -Pfx.bin=$PWD/../target/aarch64-linux-android/release/fxh`.
4. Install the APK and open it once. It asks for notifications, starts the service, and guides you through the battery settings that keep it alive.

## Test

```sh
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
node scripts/ui-check.mjs http://127.0.0.1:8787 "$(cat fx-data/token)" ./shots
```

## Updates and releases

The app checks this repository's releases twice a day, downloads a new version only after confirmation (on Wi-Fi by default), verifies its checksum, and hands it to Android's installer. See `docs/UPDATES.md`. Releases are built and signed by one automated workflow: `docs/RELEASING.md`.

## Safety model

The daemon listens on loopback only, behind a random per-install token. The Android app picks a random port, checks the daemon proves it holds the token before sending it anything, and restricts the WebView to that one address. See `SECURITY.md`.

## Licence

MIT. Web search and page reading come from the MIT-licensed [HQ](https://github.com/CalvinMagezi/hq) crates. Market data comes from an unofficial public endpoint that may change or be limited at any time; read `docs/LIMITS.md` before relying on it.
