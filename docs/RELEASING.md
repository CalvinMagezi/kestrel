# Releasing

A release is a version number, a changelog section, a tag, and one automated run that builds, signs and publishes. Nothing is built or signed by hand.

## Steps

1. Change `version` under `[workspace.package]` in `Cargo.toml`. The Android version name and code are derived from it, so there is no second place to edit.
2. Add a `## X.Y.Z` section to `CHANGELOG.md`. Plain sentences. It becomes the release notes and the "What's new" text in the app.
3. Run the gates locally: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `node scripts/ui-check.mjs ...`.
4. Commit to `main`, wait for the `ci` run to pass, then tag: `git tag vX.Y.Z && git push origin vX.Y.Z`.
5. The `release` workflow runs:
   - `verify`: the tag must equal the `Cargo.toml` version and the changelog must have the section.
   - `build`: format, lint and test again, build the daemon for Android (NDK r28c, API 28), build the APK signed with the release key, check the signature with `apksigner`, and write `SHA256SUMS`.
   - `publish`: create the GitHub release with `Kestrel-android.apk`, `SHA256SUMS`, the notes and the signing certificate's SHA-256.
6. Phones that have updates switched on see the release within twelve hours. The website's download button always points at `releases/latest/download/Kestrel-android.apk`.

## The signing key

Every release must be signed with the same key, or Android refuses to install it over the previous one and people would have to uninstall and lose their data.

- The keystore lives outside the repository, in `~/.kestrel-release/` on the maintainer's machine, and as four repository secrets: `KESTREL_KEYSTORE_B64`, `KESTREL_KEYSTORE_PASSWORD`, `KESTREL_KEY_ALIAS`, `KESTREL_KEY_PASSWORD`.
- Back the keystore up somewhere safe. If it is lost, the next release cannot be an update.
- Never commit it. The pre-commit hook, the `hygiene` job and gitleaks all refuse it.

## Rolling back

Delete or mark the release as a pre-release on GitHub, and phones stop being offered it. An installed bad version cannot be downgraded by an update: Android needs a higher version code, so ship a fixed `X.Y.(Z+1)`.

## Other automation

- `ci`: format, lint, test, secret scan, personal-data scan, repository hygiene, on every push and pull request.
- `pages`: publishes `site/` to GitHub Pages when it changes.
- `pr-lock`: closes pull requests from anyone who is not the owner or a collaborator.
- Dependabot proposes updates to Cargo, Gradle and the workflows each week.
