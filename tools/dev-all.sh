#!/bin/sh
# dev-all.sh - build the three halves of Pyren, then run the app.
#
# `bun run tauri dev` builds the frontend and `app/src-tauri`, and nothing
# else. The daemon and the widget are separate cargo workspaces on purpose
# - the daemon is a system service that must keep building on a machine
# with no GUI libraries at all - so neither is ever rebuilt by the app's
# dev loop, and the daemon is not even a child of it: systemd runs it from
# a fixed path.
#
# The failure that costs an evening is therefore silent. You change the
# daemon, restart the app, and watch the *old* daemon answer exactly as it
# did before. This builds all three and restarts what needs restarting.
#
#   tools/dev-all.sh            build everything, restart, run the app
#   tools/dev-all.sh --no-app   build and restart, then stop (for a
#                               `tauri dev` you already have running)
#
# Restarting the daemon needs root, and this asks for it with `sudo` at
# the point it is needed rather than wanting to be run as root - nothing
# else here should touch your files as root.

set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
UNIT=pyren-daemon.service
run_app=yes

for argument in "$@"; do
    case "$argument" in
        --no-app) run_app=no ;;
        -h | --help)
            sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "dev-all.sh: unknown argument '$argument' (try --help)" >&2
            exit 2
            ;;
    esac
done

say() { printf '\033[1m==> %s\033[0m\n' "$1"; }

say "daemon"
(cd "$ROOT/daemon" && cargo build)

say "widget"
(cd "$ROOT/osd" && cargo build)

# The daemon runs from a fixed path, so a fresh binary changes nothing
# until the service is restarted. Only when the unit is actually
# installed: a development daemon started by hand in another terminal is
# not ours to kill, and saying so is more useful than failing.
if systemctl list-unit-files "$UNIT" >/dev/null 2>&1 &&
    systemctl cat "$UNIT" >/dev/null 2>&1; then
    say "restarting $UNIT (needs root)"
    sudo systemctl restart "$UNIT"
else
    say "no $UNIT installed - restart your daemon yourself"
    echo "    cd daemon && sudo -E cargo run -p pyren-daemon"
fi

# The widget is single-instance and the app spawns it when none is up, so
# stopping the old one is all that is needed: the next launch of the app
# picks up the binary just built. SIGTERM, because GTK leaves on it.
if pkill -TERM -u "$(id -u)" -x pyren-osd 2>/dev/null; then
    say "stopped the old widget (the app starts the new one)"
fi

if [ "$run_app" = no ]; then
    say "done - your running 'tauri dev' still needs a restart for src-tauri"
    exit 0
fi

say "app"
cd "$ROOT/app"
exec bun run tauri dev
