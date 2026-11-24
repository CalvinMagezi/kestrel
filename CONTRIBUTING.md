# Contributing

Thanks for the interest. **This project does not accept outside pull requests**; they are closed automatically. Please open an issue to report a problem or suggest something. The rules below apply to the maintainer and to anyone given access.

- **Honest numbers.** A change that makes results look better must say why it is not look-ahead, survivorship or a cost shortcut. New detectors need a prefix test like the existing one.
- **No real orders.** Kestrel is a research instrument. A pull request that adds broker execution needs an issue and agreement first.
- **Gates.** `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, and `node scripts/ui-check.mjs` for any UI change.
- **Style.** Short functions, named constants, comments only for why. Plain, calm copy in the interface: no hype, no advice, no emoji.
- **Secrets.** Never commit a key, a token or a personal path. Keys live in a private file or the environment. Run `./scripts/install-hooks.sh` once per clone: the hook blocks secrets and personal data, and CI repeats the scan.
- **Skipped better ideas** go in `TECHDEBT.md` with the reason.

By contributing you agree your work is released under the MIT licence.
