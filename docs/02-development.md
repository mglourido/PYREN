# Development: build & run

## Prerequisites

- Rust + Cargo (`daemon/` builds with no extra system deps).
- [Bun](https://bun.sh) — `curl -fsSL https://bun.sh/install | bash`, then add
  to your shell rc:
  ```sh
  export BUN_INSTALL="$HOME/.bun"
  export PATH="$BUN_INSTALL/bin:$PATH"
  ```
- WebKitGTK + friends, required to compile `app/src-tauri` (Tauri's Linux
  webview). Arch/CachyOS:
  ```sh
  sudo pacman -S --needed webkit2gtk-4.1 base-devel curl wget file openssl \
    appmenu-gtk-module gtk3 libappindicator-gtk3 librsvg
  ```
  Other distros: <https://tauri.app/start/prerequisites/#linux>.

## 1. Run the daemon

```sh
cd daemon
cargo run -p omen-hub-daemon
```

Listens on `/tmp/omen-hub-daemon.sock` by default (unprivileged dev
fallback — see `daemon/daemon/src/main.rs`). Set `OMEN_HUB_SOCKET` to
override. Leave this running in its own terminal; the app can't do
anything useful without it.

The socket is created `0660`. Run unprivileged, that means "you and nobody
else", which is what you want for development — the app runs as the same
user. Run it under `sudo` and the daemon will say

```
omen-hub-daemon: listening on …, restricted to the daemon's own user
  note:   no 'omen-hub' group on this system, so only root can connect.
```

which is exactly what it means: your desktop user cannot reach a root
daemon until the group exists and you are in it.

```sh
sudo groupadd -f omen-hub && sudo usermod -aG omen-hub "$USER"
```

Group membership is picked up at login, so either log out and back in, or
start the test shell with `newgrp omen-hub`. `OMEN_HUB_SOCKET_GROUP`
overrides the name. Trying to connect without it fails with a permission
error the app spells out rather than swallowing.

## 2. Run the app

```sh
cd app
bun install      # first time only
bun run tauri dev
```

Without Bun installed, the same works through Node (the lockfile is Bun's,
but the dependency tree is plain npm):

```sh
node node_modules/vite/bin/vite.js dev      # frontend only, in a browser
node node_modules/@tauri-apps/cli/tauri.js dev
```

Running only the Vite dev server is the fastest loop for UI work: the app
detects it isn't inside Tauri, falls back to simulated readings and renders
every page normally (see "Demo mode" in `docs/03-frontend.md`).

This starts the Vite dev server and the Tauri/Rust shell together, then
opens the app window. First build compiles ~490 crates (WebKitGTK/GTK
bindings etc.) and takes a minute or two; rebuilds after that are fast
(only `app/src-tauri` needs recompiling).

### Wayland: window opens then immediately closes

If the window flashes open and dies with something like:

```
Gdk-Message: Error 71 (Error de protocolo) dispatching to Wayland display.
```

force GTK to run over XWayland instead of native Wayland (needs an
`X11`/XWayland `$DISPLAY` to be available, which is the default on most
desktops):

```sh
GDK_BACKEND=x11 WEBKIT_DISABLE_COMPOSITING_MODE=1 bun run tauri dev
```

This is a known WebKitGTK/compositor interaction, not specific to this
project's code — if it stops being needed on your setup, drop it.

## Checking that fan control works on a machine

`omen-hub-check` runs the same self-test as the app's Hardware check page,
as a standalone binary with no daemon, socket or GUI involved. It is the
first thing to run on an unfamiliar laptop, and the thing to paste into a
bug report.

```sh
cd daemon
cargo run -p omen-hub-check            # read-only, safe on any machine
sudo cargo run -p omen-hub-check -- --write   # also verify the PWM accepts writes
cargo run -p omen-hub-check -- --json  # machine-readable
```

Exit status is the verdict: `0` full control, `1` monitoring only, `2` no
fan-control interface. `--write` rewrites the value already set and puts
the previous mode back, so no fan changes speed.

### Running it without building the project

`tools/omen-check.sh` is the same self-test as a single POSIX shell script
with no dependencies — for a machine where building this project isn't
practical. Copy that one file across and run it:

```sh
scp tools/omen-check.sh laptop:
ssh laptop './omen-check.sh'          # or: sudo ./omen-check.sh --write
```

It performs the same checks in the same order, with the same verdicts and
exit codes. `daemon/check/tests/parity.rs` runs both against the same
fixtures and compares verdicts, exit status and per-check results, so the
two cannot drift apart silently — that test caught two real divergences the
first time it ran.

To exercise the checks without HP hardware, point it at a fixture:

```sh
mkdir -p /tmp/fake && cd /tmp/fake
echo hp > name; echo 2400 > fan1_input; echo 2550 > fan2_input
echo 128 > pwm1; echo 2 > pwm1_enable
OMEN_HUB_HWMON_DIR=/tmp/fake cargo run -p omen-hub-check -- --write
OMEN_HUB_HWMON_DIR=/tmp/fake ~/omen-hub-linux/tools/omen-check.sh --write
```

## Checking the frontend

```sh
cd app
node node_modules/svelte-check/bin/svelte-check --tsconfig ./tsconfig.json
node node_modules/vite/bin/vite.js build
```

## Sanity-checking without the GUI

The daemon's socket can be exercised directly, which is useful when
iterating on a module without waiting on a GTK rebuild:

```sh
python3 -c '
import socket, json
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect("/tmp/omen-hub-daemon.sock")
s.sendall((json.dumps({"id":1,"module":"fan","method":"getStatus"})+"\n").encode())
print(s.recv(4096).decode())
'
```

See `docs/01-ipc-protocol.md` for the full wire format.

## Continuous integration

`.github/workflows/ci.yml` runs on every push to `main`, every pull
request, and on demand. Four jobs, so a failure names the half that broke:

| job | what it runs |
|---|---|
| `daemon` | `cargo test --workspace`, `cargo clippy --all-targets -- -D warnings` |
| `app` | `bun install --frozen-lockfile`, `bun run check`, `bun run build` |
| `tauri` | `cargo check --all-targets` on `app/src-tauri`, after installing WebKitGTK |
| `shell` | `sh -n tools/omen-check.sh` |

Two things worth knowing about it:

- The daemon job runs `check/tests/parity.rs`, which invokes
  `tools/omen-check.sh` through `/bin/sh` — `dash` on the runner, rather
  than the `bash` or `zsh` it usually gets locally. That is the point of a
  POSIX script, and CI is the only place it is regularly checked.
- Nothing there can prove the socket's permissions *work*, only that the
  mode bits are right: the assertion that matters is "a second local user
  cannot connect", and CI has one user.
