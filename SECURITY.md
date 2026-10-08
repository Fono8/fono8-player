# Security policy

## Reporting a vulnerability

Please do not open a public issue for security problems. Send a report to
**github@open8.co** with:

- the affected version (`fono8 --version`) and platform,
- steps to reproduce or a proof of concept,
- the impact as you understand it.

You will get an acknowledgement within a few days. Fixes are released as a
patch version and credited in the release notes unless you prefer otherwise.

## Scope

Fono8 runs entirely on your computer. The areas worth a close look are:

- the loopback HTTP server (`src/loopback.rs`): OAuth callbacks and the Spotify
  player page on `127.0.0.1` only,
- the Google Cast relay (`src/cast/`): a single random path on the LAN
  interface that reaches the receiver,
- the browser engines for YouTube Music and Spotify (`ytm-core/`, `src/cdp.rs`,
  `fono8-web/`): injected scripts, navigation limits, cookie handling,
- the keyring storage of refresh tokens (`src/keystore.rs`),
- parsing of untrusted data: audio tags, playlist files, API responses.

Problems in the streaming services themselves (Spotify, YouTube Music, TIDAL)
should go to those services, not to this project.
