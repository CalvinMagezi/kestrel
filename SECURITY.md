# Security

Report problems privately through GitHub's "Report a vulnerability" on this repository, not in a public issue. Include what you saw and how to repeat it. This is a hobby-scale project: expect a reply within a few days, not hours.

Areas that matter most: the loopback API and its token, the Android WebView bridge, the optional model key file, and the hash-chained journal. Kestrel places no real orders, so the risks are to your data and your trust in its numbers, not to your money.

## What guards the repository
- A pre-commit hook (`scripts/install-hooks.sh`) blocks secret shapes, keystores and personal patterns before a commit exists.
- CI scans every push and pull request with gitleaks and with a personal-data scan, and fails on forbidden files such as keystores.
- A private denylist of personal patterns lives outside the repository (`~/.config/kestrel/denylist` locally, the `PERSONAL_DENYLIST` secret in CI).
- Secret scanning and push protection are switched on for the repository.
- Release signing keys are repository secrets, used only by the release workflow.
- Outside pull requests are closed automatically.
