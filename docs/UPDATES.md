# How updates work

Kestrel updates itself from this repository's GitHub releases. There is no update server of our own.

## The flow
1. Twice a day (and once shortly after start) the daemon asks GitHub for the latest release. GitHub sees the phone's IP address and the app version, like any web request. This can be switched off under Control, Updates.
2. If the release is newer, an "update available" bar appears in the app and a quiet notification is posted once.
3. Tapping Update downloads the APK. On mobile data the app waits for Wi-Fi by default, because the file is large; the owner can download anyway.
4. The daemon compares the file's SHA-256 with `SHA256SUMS` from the same release and discards it on a mismatch.
5. Tapping Install makes the app check the file again, confirm it is this app and a newer version, and hand it to Android's installer. Android shows its own confirmation and refuses anything not signed with the same key as the installed app.
6. After the update the service starts again by itself.

## What this protects against, and what it does not
- A corrupted or truncated download is caught by the checksum.
- An APK from another source cannot replace the app, because it will not carry the signing key.
- Downloads are accepted only over HTTPS from GitHub's own hosts, after any redirects.
- It does not protect against the repository owner's GitHub account or signing key being taken over. Treat the key as the crown jewel.
- Install needs the Android setting "Install unknown apps" switched on for Kestrel. The app opens that screen when needed.
