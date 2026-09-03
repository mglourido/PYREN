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
cargo run -p pyren-daemon
```

Listens on `/tmp/pyren-daemon.sock` by default (unprivileged dev
fallback — see `daemon/daemon/src/main.rs`). Set `PYREN_SOCKET` to
override. Leave this running in its own terminal; the app can't do
anything useful without it.

The socket is created `0660`. Run unprivileged, that means "you and nobody
else", which is what you want for development — the app runs as the same
user. Run it under `sudo` and the daemon will say

```
pyren-daemon: listening on …, restricted to the daemon's own user
  note:   no 'pyren' group on this system, so only root can connect.
```

which is exactly what it means: your desktop user cannot reach a root
daemon until the group exists and you are in it.

```sh
sudo groupadd -f pyren && sudo usermod -aG pyren "$USER"
```

Group membership is picked up at login, so either log out and back in, or
start the test shell with `newgrp pyren`. `PYREN_SOCKET_GROUP`
overrides the name. Trying to connect without it fails with a permission
error the app spells out rather than swallowing.

### Readings that need privilege

Not everything the daemon reports is gated on hardware. Intel publishes
integrated-GPU utilisation through the **i915 perf PMU** and nowhere else,
and opening that needs `CAP_PERFMON` — which in practice means root.
Unprivileged, the iGPU's `usagePercent` comes back `null` and the daemon
says so at startup:

```
  note:   integrated-GPU utilisation is unavailable; it needs CAP_PERFMON,
          which the systemd unit gets by running as root
```

`null` here means "we were not allowed to ask", not "the chip is idle" —
the app draws the two differently on purpose. Everything else (CPU, memory,
disks, temperatures, per-process GPU time, NVIDIA via `nvidia-smi`) works
unprivileged.

The installed systemd unit runs as root and so already carries the
capability; it also declares `AmbientCapabilities=CAP_PERFMON`, which
changes nothing today but records the requirement so that hardening the
unit later cannot quietly take the reading away.

To get it in development without installing anything:

```sh
sudo systemd-run --unit=pyren-dev \
  --setenv=PYREN_SOCKET=/tmp/pyren-root.sock \
  --setenv=PYREN_SOCKET_GROUP=wheel \
  "$PWD/target/debug/pyren-daemon"
# ... and when you are done:
sudo systemctl stop pyren-dev
```

`systemd-run` is used rather than `sudo … &` because the latter does not
reliably outlive the shell that started it. `PYREN_SOCKET_GROUP` points at
a group you are already in, so a root daemon's `0660` socket stays
reachable without creating `pyren` first.

For a permanent answer instead of a dev one, install the unit — the daemon
can do it to itself, which is also what the app's Permissions panel runs
under `pkexec`:

```sh
sudo ./target/debug/pyren-daemon --install-service
```

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

Running only the Vite dev server is the fastest loop for UI work, and it is
**not** limited to fake data: the dev server carries a bridge
(`app/dev-daemon-bridge.js`) that forwards requests from the browser down
the daemon's Unix socket, so a browser tab sees exactly what the packaged
app sees. Start the daemon first and it just works; `PYREN_SOCKET` is
honoured there too.

With no daemon running, the app falls back to simulated readings and still
renders every page (see "Demo mode" in `docs/03-frontend.md`). The bridge
is `apply: "serve"`, so it exists only in `vite dev` and never in a build.

This starts the Vite dev server and the Tauri/Rust shell together, then
opens the app window. First build compiles ~490 crates (WebKitGTK/GTK
bindings etc.) and takes a minute or two; rebuilds after that are fast
(only `app/src-tauri` needs recompiling).

### Finding the app's processes

The Rust shell runs as `pyren` (`mainBinaryName`, so `cargo run` and a
packaged build agree). Its children:

- `WebKitWebProcess` — the renderer; the one that shows CPU and most of
  the RSS.
- `WebKitNetworkProcess` — small, handles fetches.
- `glycin-svg` behind two `bwrap` sandboxes — the desktop stack's
  out-of-process, sandboxed image decoder (not ours; GDK/WebKitGTK start it
  at launch). A few MB, no ongoing CPU.

WebKitGTK hard-codes those names and exposes no way to rebrand them, so
identify them by parentage:

```sh
pgrep -P "$(pgrep -x pyren)"     # the two helper PIDs
htop -p "$(pgrep -x pyren -d,),$(pgrep -d, -P "$(pgrep -x pyren)")"
```

For a clean number over the whole app — every child stays in the
launcher's cgroup — run it in a transient scope and watch that instead:

```sh
systemd-run --user --scope --unit=pyren-app app/src-tauri/target/debug/pyren
# then, from another shell:
systemctl --user status pyren-app.scope   # aggregate CPU / memory / task count
systemd-cgtop -m | grep pyren-app         # the same, live
```

`bun run tauri dev` adds `node`/`vite`/`esbuild` to the tree; measure a
plain `cargo run --release` (or the built binary) instead when the numbers
matter.

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

## Checking what a machine can be told to do

`pyren-check` is the compatibility check: a standalone binary, no daemon,
socket or GUI involved. It is the first thing to run on an unfamiliar
laptop, and the thing to paste into a bug report.

Three sections, one verdict:

| section | question |
|---|---|
| **fans** | the same self-test the app's Hardware check page runs (`fan.diagnose`), in detail |
| **power** | what would drive the modes, whether there is a RAPL envelope, whether turbo can be switched |
| **lighting** | both RGB paths — per-key USB and the 4-zone ACPI lightbar — probed, never guessed from the model name |

```sh
cd daemon
cargo run -p pyren-check            # read-only, safe on any machine
sudo cargo run -p pyren-check -- --write   # also verify the PWM accepts writes
cargo run -p pyren-check -- --json  # machine-readable
```

The last line is the verdict, and it is `system.compatibility` — the same
one the daemon prints at startup and the app shows on its Hardware page,
from the same probes. A tool that disagreed with the app about the machine
in front of it would be worse than no tool.

**Exit status is about fans**, because that is what scripts branch on: `0`
full control, `1` monitoring only, `2` no fan-control interface. The
verdict is wider than that — a machine with no fan control can still have
power modes and a lightbar — so read the last line, not `$?`.

`--write` rewrites the value already set and puts the previous mode back,
so no fan changes speed. The lighting section issues one ACPI **read** (the
same one the daemon uses at startup) and only when `/proc/acpi/call` is
already there; it never loads a kernel module and never writes a colour.

### Running it without building the project

`tools/pyren-check.sh` is the same self-test as a single POSIX shell script
with no dependencies — for a machine where building this project isn't
practical. Copy that one file across and run it:

```sh
scp tools/pyren-check.sh laptop:
ssh laptop './pyren-check.sh'          # or: sudo ./pyren-check.sh --write
```

It performs the same checks in the same order, with the same statuses,
compatibility verdict and exit code. `daemon/check/tests/parity.rs` runs
both against the same fixtures and compares the exit status, the verdict,
`controls`, and every check in **all three** sections, so the two cannot
drift apart silently — that test caught three real divergences the first
times it ran.

To exercise the checks without HP hardware, point them at fixtures. Three
environment variables do this, and both implementations honour all three:

| variable | stands in for |
|---|---|
| `PYREN_HWMON_DIR` | the `hp-wmi` hwmon node |
| `PYREN_USB_DEVICES` | `/sys/bus/usb/devices`, for the per-key keyboard probe |
| `PYREN_ACPI_CALL` | `/proc/acpi/call`, for the lightbar probe |

```sh
mkdir -p /tmp/fake/usb/1-2 && cd /tmp/fake
echo hp > name; echo 2400 > fan1_input; echo 2550 > fan2_input
echo 128 > pwm1; echo 2 > pwm1_enable
echo 0d62 > usb/1-2/idVendor; echo 54bf > usb/1-2/idProduct
PYREN_HWMON_DIR=/tmp/fake PYREN_USB_DEVICES=/tmp/fake/usb \
  cargo run -p pyren-check -- --write
PYREN_HWMON_DIR=/tmp/fake PYREN_USB_DEVICES=/tmp/fake/usb \
  ~/pyren-linux/tools/pyren-check.sh --write
```

`PYREN_ACPI_CALL` pointed at a plain file is how the lightbar path gets
exercised on a machine with no `acpi_call`: the request is written, read
straight back, and read back is not `PASS` — so both implementations
report the firmware as having refused, and the file afterwards holds the
exact bytes each one sent. Comparing that file between the two is the
cheapest way to check they ask the firmware the same question.

## Checking the frontend

```sh
cd app
node node_modules/svelte-check/bin/svelte-check --tsconfig ./tsconfig.json
node node_modules/vite/bin/vite.js build
```

## Driving the daemon from a shell

`pyren-ctl` is a client for a running daemon, and the quickest way to
see whether a change did anything:

```sh
cd daemon
cargo run -q -p pyren-ctl -- status
cargo run -q -p pyren-ctl -- power set eco
cargo run -q -p pyren-ctl -- power tune --mode eco --pl1 35 --turbo off
cargo run -q -p pyren-ctl -- fan curve 40:20,60:50,85:100
cargo run -q -p pyren-ctl -- --json fan get
```

It reads `PYREN_SOCKET` like everything else, so it points at whichever
daemon is running. Exit status is 0, 1 when the daemon refused, 2 for bad
arguments, 3 when it could not be reached — enough to use it from a script
or a keybinding.

It is also how a *measured* power limit gets recorded: the daemon ships no
opinion about what Eco should be worth in watts on a given laptop, so
`power tune` is where a number someone actually measured goes in.

## Sanity-checking without the GUI

The socket can also be exercised directly, which is useful when adding a
method the CLI does not know about yet:

```sh
python3 -c '
import socket, json
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect("/tmp/pyren-daemon.sock")
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
| `shell` | `sh -n tools/pyren-check.sh` |

Two things worth knowing about it:

- The daemon job runs `check/tests/parity.rs`, which invokes
  `tools/pyren-check.sh` through `/bin/sh` — `dash` on the runner, rather
  than the `bash` or `zsh` it usually gets locally. That is the point of a
  POSIX script, and CI is the only place it is regularly checked.
- Nothing there can prove the socket's permissions *work*, only that the
  mode bits are right: the assertion that matters is "a second local user
  cannot connect", and CI has one user.
