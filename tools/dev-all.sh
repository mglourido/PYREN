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

# The app, the widget, and pyren-config's `ConfigStore::user()`/`::system()`
# fallback all read `$XDG_CONFIG_HOME` (see app/src-tauri/src/session.rs,
# osd/src/mode.rs, daemon/crates/config/src/lib.rs). Left unset, a dev
# `tauri dev` run reads and writes the same `~/.config/pyren` a real install
# uses, so toggling a setting here can silently rewrite the user's
# production config. Pointing it at a cache directory instead keeps dev
# runs sandboxed; it is exported before the daemon build/restart below so
# it also reaches the "run it by hand" fallback if that's used instead of
# the installed unit. The real systemd unit is unaffected: it gets its
# environment from the unit file, not from this script.
export XDG_CONFIG_HOME="${XDG_CACHE_HOME:-$HOME/.cache}/pyren-dev/config"

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

# Everything below is a fresh build replacing a previous one, and the
# previous one does not always leave cleanly: a closed terminal, a
# suspended laptop or a plain `kill` on this script does not reach the
# `cargo run`/`vite`/`pyren` tree underneath it, because none of it is a
# child of *this* script - `bun run tauri dev` runs at the very end via
# `exec`, so once that has happened there is nothing left here to signal
# them with. Left running, the old `pyren` is what actually bites:
# `tauri-plugin-single-instance` finds it alive and just refocuses its
# window, so the "fresh" run silently keeps serving yesterday's build.
#
# cwd is a precise enough signature for all of it - nothing legitimate
# runs with its cwd inside these three trees except a build or a dev run
# of this project. `$$`/`$PPID` are excluded so this never kills the
# `bun run dev:all` that is invoking it.
stop_leftovers() {
    dir=$1
    shift
    for proc in /proc/[0-9]*; do
        pid=${proc#/proc/}
        [ "$pid" = "$$" ] && continue
        [ "$pid" = "$PPID" ] && continue
        cwd=$(readlink "$proc/cwd" 2>/dev/null) || continue
        case "$cwd" in
            "$dir" | "$dir"/*) ;;
            *) continue ;;
        esac
        comm=$(cat "$proc/comm" 2>/dev/null) || continue
        for name in "$@"; do
            # Unquoted: lets a caller pass a glob (e.g. "node*"). Modern
            # Node renames its main thread to "<exe>-MainThread" and /proc
            # truncates comm to 15 bytes, so a plain "node" is reported as
            # "node-MainThread" and would never match here as a literal.
            case "$comm" in
                $name)
                    kill -TERM "$pid" 2>/dev/null &&
                        say "  stopped $comm (pid $pid, left over from before)"
                    break
                    ;;
            esac
        done
    done
}

say "cleaning up leftovers from a previous run"
stop_leftovers "$ROOT/daemon" cargo rustc
stop_leftovers "$ROOT/osd" cargo rustc
stop_leftovers "$ROOT/app/src-tauri" cargo rustc pyren
stop_leftovers "$ROOT/app" bun 'node*' vite

# Belt and braces for the vite dev server specifically: it is the one
# leftover that does not just waste a cycle but actively breaks the next
# run, because `tauri dev` cannot bind `devUrl` out from under it and
# fails outright rather than serving stale content. Covers the case a
# detached shell or `nohup` kept it alive with a cwd the check above
# never saw. Port must match `devUrl` in app/src-tauri/tauri.conf.json.
fuser -k -TERM 1420/tcp 2>/dev/null &&
    say "  freed port 1420 (a vite dev server was still holding it)"

say "daemon"
# --release, not the workspace default: the installed unit runs whatever
# `install.sh` last put at its ExecStart path, which is always a release
# build (see install.sh's `src_bin`). Restarting the unit after a plain
# `cargo build` would leave that path untouched and just relaunch
# yesterday's binary - answering exactly as it did before, silently.
(cd "$ROOT/daemon" && cargo build --release)

say "widget"
(cd "$ROOT/osd" && cargo build)

# The daemon runs from a fixed path, so a fresh binary changes nothing
# until it is both copied there and the service is restarted. Only when
# the unit is actually installed does this do either; run by hand instead
# (see --help below), it is root's and a plain user can read its cmdline -
# world-readable under /proc - without being able to read its cwd, so
# this one is matched by binary path rather than `stop_leftovers`, and
# gets the one sudo prompt it actually needs, not the daemon's own.
# Checked unconditionally, not only when no unit is installed: a daemon
# started by hand for a quick test (see --help) can otherwise keep running
# right alongside a freshly restarted service, both fighting over the same
# socket, with whichever one loses left silently answering with the old
# build - the exact failure this script exists to prevent.
dev_daemon_pid=$(pgrep -f "^$ROOT/daemon/target/(debug|release)/pyren-daemon\$" 2>/dev/null | head -1 || true)
if [ -n "$dev_daemon_pid" ]; then
    say "stopping a hand-run pyren-daemon left over from before (needs root)"
    sudo kill -TERM "$dev_daemon_pid" 2>/dev/null || true
fi

if systemctl list-unit-files "$UNIT" >/dev/null 2>&1 &&
    systemctl cat "$UNIT" >/dev/null 2>&1; then
    # Read back from the unit itself, not assumed as /usr/local/bin: a
    # user who installed with `install.sh --prefix /usr` restarts a
    # binary at a different path, and copying to the wrong one would
    # leave the unit exactly as stale as not copying at all.
    exec_path=$(systemctl cat "$UNIT" 2>/dev/null | sed -n 's/^ExecStart=//p' | head -1)
    if [ -n "$exec_path" ]; then
        say "installing the fresh build over $exec_path (needs root)"
        sudo install -Dm755 "$ROOT/daemon/target/release/pyren-daemon" "$exec_path"
    fi
    say "restarting $UNIT (needs root)"
    sudo systemctl restart "$UNIT"
else
    say "no $UNIT installed - restart your daemon yourself"
    echo "    cd daemon && sudo -E cargo run -p pyren-daemon"
    echo "    (sudo -E keeps XDG_CONFIG_HOME=$XDG_CONFIG_HOME, so it still reads dev config)"
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
