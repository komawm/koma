#!/bin/sh
# Installs Koma Wayland session units, desktop entry, portal routing, and `koma-session`
# into standard system paths on Arch Linux / CachyOS / Debian.

set -eu

PREFIX="${PREFIX:-/usr}"
SYSCONFDIR="${SYSCONFDIR:-/etc}"
SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"

install -Dm755 "$SCRIPT_DIR/koma-session" "$PREFIX/bin/koma-session"
install -Dm644 "$SCRIPT_DIR/koma.desktop" "$PREFIX/share/wayland-sessions/koma.desktop"
install -Dm644 "$SCRIPT_DIR/koma.service" "$PREFIX/lib/systemd/user/koma.service"
install -Dm644 "$SCRIPT_DIR/koma-shutdown.target" "$PREFIX/lib/systemd/user/koma-shutdown.target"
install -Dm644 "$SCRIPT_DIR/koma-portals.conf" "$PREFIX/share/xdg-desktop-portal/koma-portals.conf"
install -Dm644 "$SCRIPT_DIR/koma.portal" "$PREFIX/share/xdg-desktop-portal/portals/koma.portal"
install -Dm644 "$SCRIPT_DIR/org.freedesktop.impl.portal.desktop.koma.service" "$PREFIX/share/dbus-1/services/org.freedesktop.impl.portal.desktop.koma.service"
install -Dm644 "$SCRIPT_DIR/xdg-desktop-portal-koma.service" "$PREFIX/lib/systemd/user/xdg-desktop-portal-koma.service"

if [ ! -f "$SYSCONFDIR/greetd/config.toml" ]; then
    install -Dm644 "$SCRIPT_DIR/greetd-config.toml" "$SYSCONFDIR/greetd/config.toml"
fi

printf 'Installed Koma session files under %s (run `systemctl --user daemon-reload`).\n' "$PREFIX"
