#!/usr/bin/env bash
# Build environment for machines WITHOUT the -dev packages (no sudo needed).
#
# The native crates only need pkg-config entries and unversioned `lib*.so`
# symlinks; the runtime libraries themselves are already installed on any
# desktop. This script creates both inside target/local-sysroot and exports
# the variables cargo needs. Prefer installing the real packages (see README).
#
# Usage:  source scripts/local-env.sh && cargo build --release

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SYSROOT="$ROOT/target/local-sysroot"
LIBDIR="/usr/lib/$(gcc -print-multiarch 2>/dev/null || echo x86_64-linux-gnu)"
mkdir -p "$SYSROOT/lib" "$SYSROOT/pkgconfig"

link_lib() {
    local name="$1" soname="$2"
    if [ -e "$LIBDIR/$soname" ]; then
        ln -sf "$LIBDIR/$soname" "$SYSROOT/lib/lib$name.so"
    else
        echo "local-env: missing $LIBDIR/$soname (install the runtime package)" >&2
    fi
}

link_lib xkbcommon libxkbcommon.so.0
link_lib xkbcommon-x11 libxkbcommon-x11.so.0
link_lib asound libasound.so.2
link_lib fontconfig libfontconfig.so.1
link_lib freetype libfreetype.so.6
link_lib wayland-client libwayland-client.so.0
link_lib wayland-cursor libwayland-cursor.so.0

# GTK/WebKitGTK stack for the fono8-web helper (wry). The -sys crates only need
# link lines and a version; headers are not used (bindings are pre-generated).
stub_pc() {
    local name="$1" version="$2" libs="$3"
    cat > "$SYSROOT/pkgconfig/$name.pc" <<EOF
prefix=/usr
libdir=$SYSROOT/lib
includedir=/usr/include
Name: $name
Description: local stub for building without the -dev package
Version: $version
Libs: -L$SYSROOT/lib $libs
Cflags: -I/usr/include
EOF
}
link_lib glib-2.0 libglib-2.0.so.0
link_lib gobject-2.0 libgobject-2.0.so.0
link_lib gio-2.0 libgio-2.0.so.0
link_lib gmodule-2.0 libgmodule-2.0.so.0
link_lib cairo libcairo.so.2
link_lib cairo-gobject libcairo-gobject.so.2
link_lib pango-1.0 libpango-1.0.so.0
link_lib pangocairo-1.0 libpangocairo-1.0.so.0
link_lib gdk_pixbuf-2.0 libgdk_pixbuf-2.0.so.0
link_lib atk-1.0 libatk-1.0.so.0
link_lib gdk-3 libgdk-3.so.0
link_lib gtk-3 libgtk-3.so.0
link_lib javascriptcoregtk-4.1 libjavascriptcoregtk-4.1.so.0
link_lib webkit2gtk-4.1 libwebkit2gtk-4.1.so.0
link_lib soup-3.0 libsoup-3.0.so.0
link_lib dbus-1 libdbus-1.so.3
stub_pc glib-2.0 2.80.0 "-lglib-2.0"
stub_pc gobject-2.0 2.80.0 "-lgobject-2.0 -lglib-2.0"
stub_pc gio-2.0 2.80.0 "-lgio-2.0 -lgobject-2.0 -lglib-2.0"
stub_pc gmodule-2.0 2.80.0 "-lgmodule-2.0 -lglib-2.0"
stub_pc cairo 1.18.0 "-lcairo"
stub_pc cairo-gobject 1.18.0 "-lcairo-gobject -lcairo"
for extra in cairo-png cairo-pdf cairo-svg cairo-ps cairo-ft cairo-xlib cairo-xcb cairo-xcb-shm; do stub_pc "$extra" 1.18.0 "-lcairo"; done
stub_pc pango 1.52.0 "-lpango-1.0 -lgobject-2.0 -lglib-2.0"
stub_pc pangocairo 1.52.0 "-lpangocairo-1.0 -lpango-1.0 -lcairo"
stub_pc gdk-pixbuf-2.0 2.42.0 "-lgdk_pixbuf-2.0 -lgobject-2.0 -lglib-2.0"
stub_pc atk 2.50.0 "-latk-1.0 -lgobject-2.0 -lglib-2.0"
stub_pc gdk-3.0 3.24.30 "-lgdk-3 -lpangocairo-1.0 -lpango-1.0 -lcairo-gobject -lcairo -lgdk_pixbuf-2.0 -lgio-2.0 -lgobject-2.0 -lglib-2.0"
stub_pc gdk-x11-3.0 3.24.30 "-lgdk-3"
stub_pc gdk-wayland-3.0 3.24.30 "-lgdk-3"
stub_pc gtk+-3.0 3.24.30 "-lgtk-3 -lgdk-3 -lpangocairo-1.0 -lpango-1.0 -latk-1.0 -lcairo-gobject -lcairo -lgdk_pixbuf-2.0 -lgio-2.0 -lgobject-2.0 -lglib-2.0"
stub_pc javascriptcoregtk-4.1 2.44.0 "-ljavascriptcoregtk-4.1"
stub_pc webkit2gtk-4.1 2.44.0 "-lwebkit2gtk-4.1 -ljavascriptcoregtk-4.1 -lsoup-3.0 -lgtk-3 -lgdk-3 -lgio-2.0 -lgobject-2.0 -lglib-2.0"
stub_pc libsoup-3.0 3.4.0 "-lsoup-3.0 -lgio-2.0 -lgobject-2.0 -lglib-2.0"
stub_pc dbus-1 1.14.0 "-ldbus-1"

cat > "$SYSROOT/pkgconfig/alsa.pc" <<EOF
prefix=/usr
libdir=$SYSROOT/lib
includedir=/usr/include
Name: alsa
Description: ALSA stub for building without libasound2-dev
Version: 1.2.12
Libs: -L$SYSROOT/lib -lasound
Cflags: -I/usr/include/alsa
EOF

# A rustup toolchain without ~/.cargo/bin on PATH.
if ! command -v cargo >/dev/null 2>&1; then
    for toolchain in "$HOME"/.rustup/toolchains/stable-*; do
        [ -d "$toolchain/bin" ] && export PATH="$toolchain/bin:$PATH" && break
    done
fi

export PKG_CONFIG_PATH="$SYSROOT/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
export RUST_FONTCONFIG_DLOPEN=1
export RUSTFLAGS="-L $SYSROOT/lib${RUSTFLAGS:+ $RUSTFLAGS}"
echo "local-env: sysroot ready in $SYSROOT"
