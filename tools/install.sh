#!/bin/sh
# install.sh - put the built binaries where a machine without the build
# tree can find them.
#
# The app spawns `pyren-osd` at launch and Settings -> Services offers to
# start it at login, but nothing ever *installed* it: both look beside the
# app binary, in PATH, and in the build tree, so on any machine that is not
# this one they find nothing and say so. This is the missing step.
#
#   tools/install.sh              install whatever is built
#   tools/install.sh --uninstall  take it all back out again
#   tools/install.sh --dry-run    say what it would do, touch nothing
#
# What goes where:
#
#   pyren-osd     -> /usr/local/bin   the widget the performance key shows
#   pyren-ctl     -> /usr/local/bin   the command line, handy off the tree
#   pyren-daemon  -> /usr/local/bin   only if it was built
#   pyren-osd.service -> /usr/lib/systemd/user
#
# The unit is installed *system-wide as a user unit*, which is the thing
# `systemctl --user enable pyren-osd` wants and the app's own
# ~/.config/systemd/user copy is only a fallback for. A user unit in
# /usr/lib is offered to every session and owned by the package; the one in
# $HOME is written by the app and belongs to whoever clicked the switch.
#
# Copying into /usr needs root, and this asks for it with `sudo` at the
# point it is needed rather than wanting to be run as root: everything
# before that - finding and checking the binaries - should not be root.

set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
BIN_DIR=/usr/local/bin
UNIT_DIR=/usr/lib/systemd/user
UNIT=pyren-osd.service
action=install
dry_run=no
# What the reports say happened. A dry run must not claim it installed
# anything: the whole point of it is being believed.
did=installed
undid=removed

for argument in "$@"; do
    case "$argument" in
        --uninstall) action=uninstall ;;
        --dry-run) dry_run=yes ;;
        -h | --help)
            sed -n '2,28p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "install.sh: unknown argument '$argument' (try --help)" >&2
            exit 2
            ;;
    esac
done

if [ "$dry_run" = yes ]; then
    did="would install"
    undid="would remove"
fi

# `systemctl --user` talks to the *caller's* session manager, so under
# sudo it must drop back: run as root it would reload root's user manager,
# which is not the one that will start the widget. Skipped entirely when
# there is nobody to drop back to (a real root login, or no session).
user_daemon_reload() {
    [ "$dry_run" = yes ] && return 0
    if [ -n "${SUDO_USER:-}" ]; then
        runuser -u "$SUDO_USER" -- systemctl --user daemon-reload 2>/dev/null || true
    elif [ "$(id -u)" != 0 ]; then
        systemctl --user daemon-reload 2>/dev/null || true
    fi
}

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

# Where cargo left a binary, release for preference. Two workspaces: the
# widget is its own, for the same reason the daemon is - it needs GTK and
# the daemon must keep building on a machine with none.
built() {
    for profile in release debug; do
        candidate="$ROOT/$2/target/$profile/$1"
        if [ -x "$candidate" ]; then
            echo "$candidate"
            return 0
        fi
    done
    return 1
}

if [ "$action" = uninstall ]; then
    for binary in pyren-osd pyren-ctl pyren-daemon; do
        [ -e "$BIN_DIR/$binary" ] && as_root rm -f "$BIN_DIR/$binary" && echo "$undid $BIN_DIR/$binary"
    done
    if [ -e "$UNIT_DIR/$UNIT" ]; then
        as_root rm -f "$UNIT_DIR/$UNIT"
        echo "$undid $UNIT_DIR/$UNIT"
        user_daemon_reload
    fi
    echo
    echo "The app's own copy in ~/.config/systemd/user is left alone: the app"
    echo "wrote it and Settings -> Services is where it comes back out."
    exit 0
fi

# Nothing built is the common first-run mistake, and it is worth one clear
# sentence rather than three "not found" lines.
osd=$(built pyren-osd osd || true)
if [ -z "${osd:-}" ]; then
    echo "install.sh: no pyren-osd binary - build it first:" >&2
    echo "    cd osd && cargo build --release" >&2
    exit 1
fi

installed=0
install_binary() {
    path=$1
    name=$(basename "$path")
    as_root install -Dm755 "$path" "$BIN_DIR/$name"
    echo "$did $BIN_DIR/$name  (from ${path#"$ROOT"/})"
    installed=$((installed + 1))
}

install_binary "$osd"
for binary in pyren-ctl pyren-daemon; do
    path=$(built "$binary" daemon || true)
    [ -n "${path:-}" ] && install_binary "$path"
done

as_root install -Dm644 "$ROOT/osd/$UNIT" "$UNIT_DIR/$UNIT"
echo "$did $UNIT_DIR/$UNIT"
user_daemon_reload

echo
echo "$installed binaries. To have the widget start with the session:"
echo "    systemctl --user enable --now $UNIT"
