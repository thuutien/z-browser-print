#!/bin/sh
# Build a .deb package for Linux Mint / Ubuntu / Debian.
# Usage: ./packaging/build-deb.sh   ->   dist/z-browser-print_<version>_amd64.deb
set -e
SRC="$(cd "$(dirname "$0")/.." && pwd)"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$SRC/Cargo.toml" | head -1)
ARCH=$(dpkg --print-architecture)
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

(cd "$SRC" && cargo build --release -p zprint)

chmod 755 "$STAGE"
install -D -m 755 "$SRC/target/release/zprint" "$STAGE/usr/bin/zprint"
sed '1s|.*|#!/usr/bin/python3|' "$SRC/tools/fake_printer.py" > "$STAGE/usr/bin/zprint-fake-printer"
chmod 755 "$STAGE/usr/bin/zprint-fake-printer"
install -D -m 644 "$SRC/data/zprint.desktop" "$STAGE/usr/share/applications/zprint.desktop"
install -D -m 644 "$SRC/data/zprint-autostart.desktop" "$STAGE/etc/xdg/autostart/zprint.desktop"
install -D -m 644 "$SRC/data/zprint.svg" "$STAGE/usr/share/icons/hicolor/scalable/apps/zprint.svg"
install -D -m 644 "$SRC/data/zprint-64.png" "$STAGE/usr/share/icons/hicolor/64x64/apps/zprint.png"
install -D -m 644 "$SRC/README.md" "$STAGE/usr/share/doc/z-browser-print/README.md"

install -d "$STAGE/DEBIAN"
cat > "$STAGE/DEBIAN/control" <<CTRL
Package: z-browser-print
Version: $VERSION
Section: utils
Priority: optional
Architecture: $ARCH
Depends: libc6, libgcc-s1, libgl1, libegl1, libx11-6, libxcursor1, libxrandr2, libxi6, libxkbcommon0, libxkbcommon-x11-0
Recommends: cups-client, xdg-utils, poppler-utils
Suggests: python3
Replaces: zprint
Conflicts: zprint
Maintainer: Z Browser Print <zbrowserprint@localhost>
Description: Z Browser Print - Linux alternative to Zebra Browser Print
 Lets web pages print raw ZPL to Zebra label printers. Runs in the tray and
 answers both its own API (port 9180) and Zebra's BrowserPrint JS library
 (port 9100), so web apps written for Zebra Browser Print work unchanged.
 Prints over the network (port 9100), to USB printers (/dev/usb/lp*) and
 through raw CUPS queues. Includes zprint-fake-printer for testing.
CTRL
cat > "$STAGE/DEBIAN/conffiles" <<CONF
/etc/xdg/autostart/zprint.desktop
CONF
cat > "$STAGE/DEBIAN/postinst" <<'POST'
#!/bin/sh
set -e
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t /usr/share/icons/hicolor || true
command -v update-desktop-database >/dev/null && update-desktop-database -q /usr/share/applications || true
POST
chmod 755 "$STAGE/DEBIAN/postinst"
cp "$STAGE/DEBIAN/postinst" "$STAGE/DEBIAN/postrm"
mkdir -p "$SRC/dist"
dpkg-deb --root-owner-group --build "$STAGE" "$SRC/dist/z-browser-print_${VERSION}_${ARCH}.deb"
