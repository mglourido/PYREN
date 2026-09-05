#!/bin/sh
# install.sh - install Pyren so a machine without the build tree can run it.
#
# Pyren is a handful of binaries with three lifetimes:
#
#   pyren-daemon   root, a systemd service, started at boot
#   pyren          the desktop app, your session
#   pyren-osd      the performance-key widget, your session (a user service)
#   pyren-ctl      the shell client, run on demand
#   pyren-check    the standalone compatibility probe, run on demand
#
# plus the patched hp-wmi driver sources, which the daemon's own installer
# stages and builds only if this board needs them.
#
# This puts all of that where each piece is looked for, writes the daemon's
# systemd unit (via `pyren-daemon --install-service`), creates the `pyren`
# group the daemon's socket is handed to, and adds you to it.
#
#   sudo ./install.sh                 install everything
#   sudo ./install.sh --no-service    lay the files down, touch no units
#   sudo ./install.sh --prefix /usr   install under /usr, not /usr/local
#   sudo ./install.sh --driver-dir D  put the driver sources at D, not
#                                     /usr/share/pyren/driver (needs
#                                     PYREN_DRIVER_DIR=D for the daemon)
#   sudo ./install.sh --uninstall     take it all back out
#   ./install.sh --dry-run            say what it would do, change nothing
#
# It runs from an unpacked release archive (bin/, share/, lib/ beside this
# script) and, in a checkout as install/install.sh, from a release build in
# each of daemon/, osd/ and app/ (see "Install from a source checkout" in
# INSTALL.md).
#
# Root is needed only to write under /usr and /etc; this asks for it with
# `sudo` at each step rather than wanting to be run as root, so finding and
# checking the files first is never privileged.

set -eu

SELF=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

PREFIX=/usr/local
# Not under --prefix by default: the daemon looks for the driver sources at
# this exact path (daemon/crates/installer/src/detect.rs, find_driver_source).
# --driver-dir moves them; the daemon then needs PYREN_DRIVER_DIR set to match.
DRIVER_DEST=/usr/share/pyren/driver
driver_dir_set=no
action=install
dry_run=no
with_service=yes
purge=no

while [ $# -gt 0 ]; do
    case "$1" in
    --prefix) PREFIX=$2; shift 2 ;;
    --driver-dir) DRIVER_DEST=$2; driver_dir_set=yes; shift 2 ;;
    --uninstall) action=uninstall; shift ;;
    --no-service) with_service=no; shift ;;
    --purge) purge=yes; shift ;;
    --dry-run) dry_run=yes; shift ;;
    -h | --help)
        sed -n '2,35p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "install.sh: unknown argument '$1' (try --help)" >&2
        exit 2
        ;;
    esac
done

BIN_DEST="$PREFIX/bin"
APP_DEST="$PREFIX/share/applications"
ICON_DEST="$PREFIX/share/icons/hicolor"
USER_UNIT_DEST="$PREFIX/lib/systemd/user"
OSD_UNIT=pyren-osd.service
BINARIES="pyren pyren-daemon pyren-ctl pyren-check pyren-osd"

say() { printf '\033[1m==> %s\033[0m\n' "$1"; }

# A confirming line after a real action; nothing in a dry run, where
# as_root() has already printed "would run: ...".
done_line() { [ "$dry_run" = yes ] || echo "  $*"; }

# Root only where it is needed, and not at all for a dry run.
as_root() {
    if [ "$dry_run" = yes ]; then
        echo "  would run: $*"
    elif [ "$(id -u)" = 0 ]; then
        "$@"
    else
        sudo "$@"
    fi
}

# `systemctl --user` talks to the caller's session manager, so under sudo it
# has to drop back to the user who invoked it - root's user manager is not
# the one that will start the widget. Skipped when there is nobody to drop
# back to (a real root login, or no session).
user_systemctl() {
    if [ "$dry_run" = yes ]; then
        echo "  would run: systemctl --user $*"
        return 0
    fi
    if [ -n "${SUDO_USER:-}" ]; then
        runuser -u "$SUDO_USER" -- systemctl --user "$@" 2>/dev/null || true
    elif [ "$(id -u)" != 0 ]; then
        systemctl --user "$@" 2>/dev/null || true
    fi
}

# The user this install is for: whoever ran sudo, else the current user.
target_user() { echo "${SUDO_USER:-$(id -un)}"; }

# Rebuild the desktop and icon caches so they match what is on disk now -
# after an install and after an uninstall (a stale entry pointing at a
# removed .desktop is as wrong as a missing one). Best-effort: a missing
# tool is not an error.
refresh_caches() {
    [ "$dry_run" = yes ] && return 0
    if command -v update-desktop-database >/dev/null 2>&1; then
        as_root update-desktop-database "$APP_DEST" 2>/dev/null || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        as_root gtk-update-icon-cache -qtf "$PREFIX/share/icons/hicolor" 2>/dev/null || true
    fi
}

# --- locate what we are installing -----------------------------------------
#
# Two layouts. A release archive has bin/ share/ lib/ beside this script; a
# checkout has each binary under its own target/release and the data files
# in their source locations.

if [ -x "$SELF/bin/pyren-daemon" ]; then
    MODE=archive
    BASE=$SELF
elif [ -x "$SELF/../daemon/target/release/pyren-daemon" ]; then
    MODE=checkout
    BASE=$(CDPATH= cd -- "$SELF/.." && pwd)
else
    echo "install.sh: nothing to install." >&2
    echo "  From a checkout, build a release first:" >&2
    echo "    (cd daemon && cargo build --release) && \\" >&2
    echo "    (cd osd && cargo build --release) && \\" >&2
    echo "    (cd app && bun install && bun run tauri build --no-bundle)" >&2
    echo "  or run tools/release.sh and install from the archive it makes." >&2
    exit 1
fi

src_bin() {
    if [ "$MODE" = archive ]; then
        echo "$BASE/bin/$1"
        return
    fi
    case "$1" in
    pyren) echo "$BASE/app/src-tauri/target/release/pyren" ;;
    pyren-osd) echo "$BASE/osd/target/release/pyren-osd" ;;
    *) echo "$BASE/daemon/target/release/$1" ;;
    esac
}

if [ "$MODE" = archive ]; then
    DRIVER_SRC="$BASE/share/pyren/driver"
    DESKTOP_SRC="$BASE/share/applications/pyren.desktop"
    OSD_UNIT_SRC="$BASE/lib/systemd/user/$OSD_UNIT"
    ICON_SRC="$BASE/share/icons/hicolor"
else
    DRIVER_SRC="$BASE/driver"
    DESKTOP_SRC="$BASE/install/pyren.desktop"
    OSD_UNIT_SRC="$BASE/osd/$OSD_UNIT"
    ICON_SRC="$BASE/app/src-tauri/icons"
fi

rel() { echo "${1#"$BASE"/}"; }

# --- uninstall -----------------------------------------------------------

if [ "$action" = uninstall ]; then
    if [ "$with_service" = yes ] && { [ -x "$BIN_DEST/pyren-daemon" ] || [ "$dry_run" = yes ]; }; then
        say "systemd unit + group"
        as_root "$BIN_DEST/pyren-daemon" --remove-service || true
    fi

    say "binaries"
    for b in $BINARIES; do
        if [ -e "$BIN_DEST/$b" ] || [ "$dry_run" = yes ]; then
            as_root rm -f "$BIN_DEST/$b"
            done_line "removed $BIN_DEST/$b"
        fi
    done

    say "data files"
    for p in \
        "$DRIVER_DEST" \
        "$APP_DEST/pyren.desktop" \
        "$ICON_DEST/32x32/apps/pyren.png" \
        "$ICON_DEST/128x128/apps/pyren.png" \
        "$ICON_DEST/256x256/apps/pyren.png" \
        "$ICON_DEST/scalable/apps/pyren.svg" \
        "$USER_UNIT_DEST/$OSD_UNIT"; do
        if [ -e "$p" ] || [ "$dry_run" = yes ]; then
            as_root rm -rf "$p"
            done_line "removed $p"
        fi
    done
    as_root rmdir "$(dirname "$DRIVER_DEST")" 2>/dev/null || true
    refresh_caches
    user_systemctl daemon-reload

    if [ "$purge" = yes ]; then
        say "config (--purge)"
        as_root rm -rf /etc/pyren
        home=$(getent passwd "$(target_user)" | cut -d: -f6)
        [ -n "$home" ] && as_root rm -rf "$home/.config/pyren"
    else
        echo
        echo "Left in place: /etc/pyren, ~/.config/pyren (your settings) and the"
        echo "'pyren' group. Add --purge to remove the settings too."
    fi
    exit 0
fi

# --- install -----------------------------------------------------------

say "checking the pieces ($MODE)"
missing=
for b in $BINARIES; do
    p=$(src_bin "$b")
    if [ -x "$p" ]; then
        echo "  ok       $(rel "$p")"
    else
        echo "  MISSING  $p" >&2
        missing=yes
    fi
done
for p in "$DRIVER_SRC/dkms.conf" "$DRIVER_SRC/hp-wmi-omen/hp-wmi.c" "$DESKTOP_SRC" "$OSD_UNIT_SRC"; do
    if [ -e "$p" ]; then
        echo "  ok       $(rel "$p")"
    else
        echo "  MISSING  $p" >&2
        missing=yes
    fi
done
if [ -n "$missing" ]; then
    echo "install.sh: some pieces are missing (see above)." >&2
    exit 1
fi

say "binaries -> $BIN_DEST"
for b in $BINARIES; do
    as_root install -Dm755 "$(src_bin "$b")" "$BIN_DEST/$b"
    done_line "installed $BIN_DEST/$b"
done

say "driver sources -> $DRIVER_DEST"
as_root rm -rf "$DRIVER_DEST"
as_root mkdir -p "$DRIVER_DEST"
# The contents, not the directory, so DRIVER_DEST is exactly the tree the
# daemon expects (dkms.conf and hp-wmi-omen/ at its top level).
as_root cp -R "$DRIVER_SRC/." "$DRIVER_DEST/"
done_line "installed $DRIVER_DEST"

say "desktop entry + icons"
as_root install -Dm644 "$DESKTOP_SRC" "$APP_DEST/pyren.desktop"
done_line "installed $APP_DEST/pyren.desktop"
if [ "$MODE" = archive ]; then
    for r in 32x32/apps/pyren.png 128x128/apps/pyren.png 256x256/apps/pyren.png scalable/apps/pyren.svg; do
        [ -e "$ICON_SRC/$r" ] || continue
        as_root install -Dm644 "$ICON_SRC/$r" "$ICON_DEST/$r"
        done_line "installed $ICON_DEST/$r"
    done
else
    as_root install -Dm644 "$ICON_SRC/32x32.png" "$ICON_DEST/32x32/apps/pyren.png"
    as_root install -Dm644 "$ICON_SRC/128x128.png" "$ICON_DEST/128x128/apps/pyren.png"
    [ -e "$ICON_SRC/128x128@2x.png" ] && as_root install -Dm644 "$ICON_SRC/128x128@2x.png" "$ICON_DEST/256x256/apps/pyren.png"
    [ -e "$ICON_SRC/icon.svg" ] && as_root install -Dm644 "$ICON_SRC/icon.svg" "$ICON_DEST/scalable/apps/pyren.svg"
    done_line "installed pyren icons under $ICON_DEST"
fi
refresh_caches

say "widget user unit -> $USER_UNIT_DEST/$OSD_UNIT"
if [ "$dry_run" = yes ]; then
    echo "  would install $USER_UNIT_DEST/$OSD_UNIT (ExecStart=$BIN_DEST/pyren-osd)"
else
    tmp=$(mktemp)
    sed "s|^ExecStart=.*|ExecStart=$BIN_DEST/pyren-osd|" "$OSD_UNIT_SRC" >"$tmp"
    as_root install -Dm644 "$tmp" "$USER_UNIT_DEST/$OSD_UNIT"
    rm -f "$tmp"
    echo "  installed $USER_UNIT_DEST/$OSD_UNIT"
fi
user_systemctl daemon-reload

if [ "$with_service" = yes ]; then
    say "daemon systemd unit + 'pyren' group"
    # Writes /etc/systemd/system/pyren-daemon.service pointing at the binary
    # just installed, runs `groupadd -f pyren`, reloads, enables --now.
    as_root "$BIN_DEST/pyren-daemon" --install-service

    tu=$(target_user)
    if [ "$tu" != root ]; then
        say "adding $tu to the 'pyren' group"
        as_root usermod -aG pyren "$tu"
    else
        echo "  running as root with no SUDO_USER - add your desktop user yourself:"
        echo "     sudo usermod -aG pyren \$USER"
    fi
else
    echo
    echo "--no-service: the daemon unit and the 'pyren' group were NOT set up."
    echo "When ready:  sudo $BIN_DEST/pyren-daemon --install-service"
    echo "             sudo usermod -aG pyren \$USER"
fi

if [ "$driver_dir_set" = yes ]; then
    echo
    echo "Note: driver sources went to $DRIVER_DEST, not the default."
    echo "The daemon only finds them if PYREN_DRIVER_DIR=$DRIVER_DEST is in its"
    echo "environment - add it to the unit with 'systemctl edit pyren-daemon'."
fi

echo
say "done"
cat <<EOF
Two things left, both in your own session:

  1. Group membership is picked up at login. Log out and back in, or run
     'newgrp pyren' in a shell, before the app or pyren-ctl can reach the
     daemon. Until then you get a permission error.

  2. To start the performance-key widget with your session:
       systemctl --user enable --now $OSD_UNIT

Then launch Pyren from your app menu, or run 'pyren'. 'pyren-check' reports
what this laptop can be told to do.
EOF
