#!/usr/bin/env bash
# Download the AppImage tools pinned in packaging/appimage-tools.json into build/tools/,
# checking each SHA-256 before making the file executable.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tools="$root/build/tools"
mkdir -p "$tools"
python3 - "$root/packaging/appimage-tools.json" <<'PY' | while read -r name url sha; do
import json, sys
for name, tool in json.load(open(sys.argv[1])).items():
    print(name, tool["url"], tool["sha256"])
PY
  file="$tools/$name"
  if [ ! -f "$file" ] || ! echo "$sha  $file" | sha256sum -c --status; then
    curl -fsSL --retry 3 -o "$file.part" "$url"
    echo "$sha  $file.part" | sha256sum -c --status || { echo "checksum mismatch: $name" >&2; rm -f "$file.part"; exit 1; }
    mv "$file.part" "$file"
  fi
  chmod +x "$file"
  echo "$file"
done
