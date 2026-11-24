# Tech debt

| Item | Reason skipped | Date | File |
|---|---|---|---|
| Share very large exports through a file provider instead of a capped text share | Needs AndroidX or a custom provider; the cap covers normal use | 2026-11-24 | android/app/src/main/kotlin/dev/fxharness/app/MainActivity.kt |
| A second, user-supplied candle source (CSV import) so Yahoo is not a single point of failure | Time; the cache covers short outages | 2026-11-24 | crates/fx-feed/src/yahoo.rs |
| Interface in other languages (Luganda, Swahili) | Needs native translators; strings are still inline in page.html | 2026-11-24 | crates/fx-harness/src/page.html |
| Session clock (London and New York opens) | Needs daylight-saving rules for two zones; low value for the current strategy | 2026-11-24 | crates/fx-harness/src/api.rs |
| Higher-timeframe bias in the strategy | One timeframe at a time keeps the first strategy auditable | 2026-11-24 | crates/fx-core/src/strategy.rs |
| Android build in CI | Needs the NDK on the runner | 2026-11-24 | .github/workflows/ci.yml |
| Rename the `fx-*` crates and the `dev.fxharness.app` application id to the public name | Working names; renaming churns every path | 2026-11-24 | Cargo.toml, android/app/build.gradle.kts |
| Stream the strategy state instead of replaying the window on each poll | Replay is cheap at 3,000 bars and provably matches an uninterrupted run | 2026-11-24 | crates/fx-harness/src/app.rs |
