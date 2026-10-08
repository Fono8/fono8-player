# Releasing Fono8

Releases are built by GitHub Actions (the **Desktop builds** workflow,
`.github/workflows/build.yml`) for:

| Platform | Runner | Files |
| --- | --- | --- |
| Linux x86_64 | Ubuntu 22.04 (glibc baseline) | `Fono8-<version>-x86_64.AppImage`, `Fono8-<version>-linux-x86_64.tar.gz` |
| Windows x64 | windows-2022 | `Fono8-<version>-windows-x86_64.zip` (`Fono8/fono8.exe`, `Fono8/fono8-web.exe`) |
| macOS | — | not yet enabled; the matrix entries are commented out, `scripts/package.sh` can already build a `.dmg` |

Every file comes with a `.sha256`. The workflow runs the tests, builds the
release binaries, packages them, verifies the checksums and starts every package
with `--version` (on Windows from a directory with spaces and non-ASCII
characters).

What runs when:

| Trigger | What runs |
| --- | --- |
| Pull request to `main`, push to `main` | `cargo test` on Linux and Windows, no packages |
| Tag `v<version>` on `main` | tests, packages, draft release |
| **Run workflow** (on demand) | tests and packages, no release |

Pushes to `main` run only the tests; the tag push builds the packages. A tag
whose commit is not on `main` stops the workflow. The packages are under
**Artifacts** of each run.

## Making a release

1. Set the version in `Cargo.toml` (`version = "0.1.0"` or e.g. `"0.1.0-beta.1"`),
   run `cargo build` to update `Cargo.lock`, and commit.
2. Create and push the tag `v<version>` (it must match the version in
   `Cargo.toml`, otherwise the workflow stops):

   ```bash
   git tag v0.1.0-beta.1
   git push origin v0.1.0-beta.1
   ```

3. When the workflow finishes, a **draft release** (prerelease) with all files
   and release notes is waiting on GitHub. The notes end with **What's
   changed**: the commit subjects since the previous tag (without the
   `chore(release)` bump), so write subjects that make sense to users. Edit the
   list in the draft if needed.
4. Check by hand before publishing:
   - Linux: the AppImage starts after `chmod +x`, after the first start Fono8 is
     in the application menu with its icon, a local file plays, the tray works.
   - Windows 10/11: the whole ZIP extracted, `fono8.exe` starts without a console
     window, the taskbar icon is right, a local file plays, YouTube Music works
     through WebView2.
   - Spotify (Linux, with Chrome/Edge/Brave installed) and the TIDAL import on a
     test account.
5. Publish the draft (**Publish release**).

## Locally (Linux)

```bash
source scripts/local-env.sh   # only without -dev packages, see README
cargo build --locked --release --workspace --target x86_64-unknown-linux-gnu
scripts/fetch-appimage-tools.sh
APPIMAGETOOL=$PWD/build/tools/appimagetool-x86_64.AppImage \
APPIMAGE_RUNTIME=$PWD/build/tools/runtime-x86_64 \
  scripts/package.sh x86_64-unknown-linux-gnu linux-x86_64
```

The AppImage tools are pinned by URL and SHA-256 in
`packaging/appimage-tools.json`; the script checks
the checksum before using them. Change both fields together.
