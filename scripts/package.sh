#!/usr/bin/env bash
# Package the release binary for one platform into dist/.
#
#   scripts/package.sh <rust-target> <artifact-name>
#
# Linux   -> Fono8-<version>-<name>.tar.gz (binary, .desktop, icons, README, LICENSE)
#            and Fono8-<version>-x86_64.AppImage when APPIMAGETOOL and APPIMAGE_RUNTIME
#            point to the tools from scripts/fetch-appimage-tools.sh
# macOS   -> Fono8-<version>-<name>.dmg with Fono8.app (ad-hoc signed, LICENSE and
#            notices in Contents/Resources) and a link to /Applications
# Windows -> Fono8-<version>-<name>.zip (fono8.exe, fono8-web.exe, README, LICENSE)
# Every package carries LICENSE (GPL-3.0-or-later) and, when generated with
# `cargo about generate about.hbs -o THIRD_PARTY_NOTICES.md`, the dependency licenses.
# Every artifact gets a <file>.sha256 next to it.
set -euo pipefail

target="${1:?rust target}"
name="${2:?artifact name}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="$(grep -m1 '^version' "$root/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
dist="$root/dist"
stage="$root/target/package/$name"
rm -rf "$stage"
mkdir -p "$dist" "$stage"

case "$target" in
  *-linux-*)
    bin="$root/target/$target/release/fono8"
    app="$stage/Fono8-$version"
    mkdir -p "$app/bin" "$app/share/applications" "$app/share/icons/hicolor/scalable/apps"
    cp "$bin" "$app/bin/fono8"
    cp "$root/target/$target/release/fono8-web" "$app/bin/fono8-web"
    cp "$root/packaging/fono8.desktop" "$app/share/applications/"
    cp "$root/assets/fono8.svg" "$app/share/icons/hicolor/scalable/apps/fono8.svg"
    for size in 16 32 48 64 128 256 512; do
      mkdir -p "$app/share/icons/hicolor/${size}x${size}/apps"
      cp "$root/assets/app-icon/fono8-$size.png" "$app/share/icons/hicolor/${size}x${size}/apps/fono8.png"
    done
    cp "$root/README.md" "$root/LICENSE" "$app/"
    [ -f "$root/THIRD_PARTY_NOTICES.md" ] && cp "$root/THIRD_PARTY_NOTICES.md" "$app/"
    if command -v desktop-file-validate >/dev/null; then
      desktop-file-validate "$app/share/applications/fono8.desktop"
    fi
    tar -C "$stage" -czf "$dist/Fono8-$version-$name.tar.gz" "Fono8-$version"
    if [ -n "${APPIMAGETOOL:-}" ]; then
      appdir="$stage/Fono8.AppDir"
      mkdir -p "$appdir/usr"
      cp -r "$app/bin" "$app/share" "$appdir/usr/"
      mkdir -p "$appdir/usr/share/doc/fono8"
      cp "$app"/README.md "$app"/LICENSE "$appdir/usr/share/doc/fono8/"
      [ -f "$app/THIRD_PARTY_NOTICES.md" ] && cp "$app/THIRD_PARTY_NOTICES.md" "$appdir/usr/share/doc/fono8/"
      cp "$root/packaging/fono8.desktop" "$appdir/fono8.desktop"
      cp "$root/assets/app-icon/fono8-256.png" "$appdir/fono8.png"
      ln -sf fono8.png "$appdir/.DirIcon"
      printf '%s\n' '#!/bin/sh' 'HERE="$(dirname "$(readlink -f "$0")")"' 'exec "$HERE/usr/bin/fono8" "$@"' > "$appdir/AppRun"
      chmod +x "$appdir/AppRun"
      ARCH=x86_64 APPIMAGE_EXTRACT_AND_RUN=1 "$APPIMAGETOOL" --no-appstream --runtime-file "$APPIMAGE_RUNTIME" \
        "$appdir" "$dist/Fono8-$version-x86_64.AppImage"
    fi
    ;;
  *-apple-darwin)
    bin="$root/target/$target/release/fono8"
    volume="$stage/volume"
    app="$volume/Fono8.app"
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    cp "$bin" "$app/Contents/MacOS/fono8"
    cp "$root/target/$target/release/fono8-web" "$app/Contents/MacOS/fono8-web"
    sed "s/__VERSION__/$version/g" "$root/packaging/Info.plist" > "$app/Contents/Info.plist"
    cp "$root/README.md" "$root/LICENSE" "$app/Contents/Resources/"
    [ -f "$root/THIRD_PARTY_NOTICES.md" ] && cp "$root/THIRD_PARTY_NOTICES.md" "$app/Contents/Resources/"
    iconset="$stage/fono8.iconset"
    mkdir -p "$iconset"
    for size in 16 32 128 256 512; do
      cp "$root/assets/app-icon/fono8-$size.png" "$iconset/icon_${size}x${size}.png"
      double=$((size * 2))
      cp "$root/assets/app-icon/fono8-$double.png" "$iconset/icon_${size}x${size}@2x.png"
    done
    iconutil -c icns "$iconset" -o "$app/Contents/Resources/fono8.icns"
    # Ad-hoc signature so Gatekeeper at least sees a consistent bundle; notarization is a later step.
    codesign --force --deep --sign - "$app"
    # Drag-and-drop install: the volume shows Fono8.app next to a link to /Applications.
    ln -s /Applications "$volume/Applications"
    rm -f "$dist/Fono8-$version-$name.dmg"
    hdiutil create -volname "Fono8" -srcfolder "$volume" -ov -format UDZO "$dist/Fono8-$version-$name.dmg"
    ;;
  *-windows-*)
    bin="$root/target/$target/release/fono8.exe"
    app="$stage/Fono8"
    mkdir -p "$app"
    cp "$bin" "$app/fono8.exe"
    cp "$root/target/$target/release/fono8-web.exe" "$app/fono8-web.exe"
    cp "$root/README.md" "$root/LICENSE" "$app/"
    [ -f "$root/THIRD_PARTY_NOTICES.md" ] && cp "$root/THIRD_PARTY_NOTICES.md" "$app/"
    (cd "$stage" && 7z a -tzip "$dist/Fono8-$version-$name.zip" Fono8 >/dev/null)
    ;;
  *)
    echo "unsupported target: $target" >&2
    exit 1
    ;;
esac
# Checksums for the files built by this run.
(
  cd "$dist"
  for file in Fono8-"$version"-*; do
    case "$file" in *.sha256) continue ;; esac
    # macOS has no sha256sum; shasum -a 256 prints the same "<hash>  <file>" line.
    if command -v sha256sum >/dev/null; then sha256sum "$file"; else shasum -a 256 "$file"; fi > "$file.sha256"
  done
)
ls -la "$dist"
