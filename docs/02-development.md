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
- GTK 4 and gtk4-layer-shell, required to compile `osd/` — and *only*
  `osd/`, which is why it is a separate workspace. Arch/CachyOS:
  ```sh
  sudo pacman -S --needed gtk4 gtk4-layer-shell
  ```
  Debian/Ubuntu: `libgtk-4-dev libgtk4-layer-shell-dev`.

## 0. All three at once

Pyren is three programs with three lifetimes, and the sections below run
them one at a time. In day-to-day work you usually want all of them, on
the code you just wrote:

```sh
cd app
bun run dev:all      # builds daemon + widget, restarts them, runs the app
bun run dev:deps     # the same without the app, for a 'tauri dev' already up
```

That is `tools/dev-all.sh`. It asks for `sudo` once, to restart the
daemon's service, and for nothing else.

**Why it needs to exist.** `bun run tauri dev` builds the frontend and
`app/src-tauri`, and *nothing else*. The daemon and the widget are
separate cargo workspaces — deliberately, so the daemon keeps building on
a machine with no GUI libraries at all — so neither is rebuilt by the
app's dev loop. Worse, the daemon is not a child of the app: systemd
starts it from a fixed path, so even a rebuilt binary changes nothing
until the service restarts.

The result is a silent failure rather than an error. You change the
daemon, restart the app, and watch the old daemon answer exactly as it
did before, with nothing anywhere saying why. What is safe to assume:

| you changed | `tauri dev` rebuilds it | to see it |
|---|---|---|
| frontend (`app/src`) | yes | it reloads on save |
| `app/src-tauri` | yes | restart `tauri dev` |
| `daemon/` | **no** | `cargo build`, then `sudo systemctl restart pyren-daemon` |
| `osd/` | **no** | `cargo build`, then stop `pyren-osd` (the app respawns it) |

To check by hand whether what is *running* is what you last built:

```sh
systemctl show pyren-daemon -p ExecMainStartTimestamp   # started when?
ls -l --time-style=+%T daemon/target/debug/pyren-daemon # built when?
```

## 1. Run the daemon

```sh
cd daemon
cargo run -p pyren-daemon
```

Listens on `/tmp/pyren-daemon.sock` by default (unprivileged dev
fallback — see `daemon/daemon/src/main.rs`). Set `PYREN_SOCKET` to
override. Leave this running in its own terminal; the app can't do
anything useful without it.

`PYREN_LOG` sets how much it says — `off`, `error`, `warn`, `info` (the
default) or `debug`. It filters *logging* only: the startup report and
`--check` are output somebody asked for, and are printed whatever the
level says, so `PYREN_LOG=warn` under systemd leaves the journal with the
report and the things that went wrong.

```sh
PYREN_LOG=debug cargo run -p pyren-daemon
```

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

That last parenthesis is also the trap: `app/src-tauri` is the *only*
Rust it recompiles. A change in `daemon/` or `osd/` is not part of this
loop at all — see "All three at once" above, or `bun run dev:all`.

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

## 3. Run the on-screen display

**The app starts it for you.** It looks for the widget beside its own
binary, in `PATH`, and - because that is the case that matters here - at
`osd/target/<profile>/pyren-osd` in this tree, then spawns it unless a
`pyren-osd` of yours is already running. So `cargo build` in `osd/` once
and it comes up with the app from then on.

To run it by hand instead:

```sh
cd osd
cargo run -- --show     # --show draws it once, so you can see it without a key
```

Then teach the daemon which key is yours and press it:

```sh
pyren-ctl hotkey learn      # press Fn+P while it waits
pyren-ctl hotkey get        # what it caught, and whether it is being heard
```

`hotkey learn` needs a daemon that can read `/dev/input`, which means a
root one — `cargo run -p pyren-daemon` as yourself will answer
`permissionDenied`, correctly. What still works unprivileged is everything
downstream of the key:

```sh
pyren-ctl hotkey press      # run the action as though the key were pressed
pyren-ctl events            # watch what the daemon publishes, as it happens
```

`hotkey press` is the one to develop the widget against: it goes through
the same action, publishes the same events, and needs no hardware. Two
things worth knowing while working on the widget:

- **It is a layer-shell surface**, so it will not show up in
  `hyprctl clients`. `hyprctl layers` is where it is, under the namespace
  `pyren-osd`, on the `overlay` level.
- **A second launch does not start a second process.** It activates the
  running one, which shows the widget — which also makes `pyren-osd` a
  reasonable thing to put on a compositor keybinding. Note that this path
  only ever *shows*; it is the shortcut (a `hotkey.pressed` event) that
  toggles, so `hotkey press` twice opens and then closes the widget while
  launching `pyren-osd` twice shows it twice.

To develop against a daemon of your own without disturbing the installed
one, give both a socket and a config directory of their own:

```sh
PYREN_SOCKET=/tmp/pyren-dev.sock PYREN_CONFIG_DIR=/tmp/pyren-dev cargo run -p pyren-daemon
PYREN_SOCKET=/tmp/pyren-dev.sock cargo run          # in osd/
```

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

## Testing the performance profiles

The four modes are the one feature whose interesting half *writes* -
`/sys/firmware/acpi/platform_profile`, `/sys/class/powercap` and
`powerprofilesctl` - so testing it means deciding whose machine gets
changed. There are two suites, and they answer different questions.

**The hermetic one** drives a laptop that does not exist:

```sh
cd daemon && cargo test -p pyren-power --test profiles
```

`crates/power/tests/profiles.rs` builds a fixture sysfs tree plus a
stand-in `powerprofilesctl` that records every request, and points the
module at it through `PYREN_PLATFORM_PROFILE`, `PYREN_CPU_ROOT`,
`PYREN_POWERCAP` and `PYREN_POWERPROFILESCTL`. Because the fake machine
is readable back, assertions are exact: *Eco set the firmware profile to
`low-power` and asked the OS for `power-saver`*. It covers switching
between the four (including twenty rounds of cycling, and ten threads
doing it at once), the app closing, the daemon restarting with and
without restore-on-start, the stock-envelope ratchet across five boots,
and simulated timelines of up to forty minutes of supervision.

Those timelines run on a simulated clock. The one that does not is
ignored by default:

```sh
PYREN_SOAK_SECS=300 cargo test -p pyren-power --test profiles -- --ignored --nocapture
```

That runs the actual supervisor thread against the fixture for five
minutes and asserts at every sample that the daemon and the machine
agree, and that it is not flapping.

**The live one** is the half a fixture cannot honestly answer - the real
firmware, and the two lifecycle events:

```sh
tools/power-soak.sh                # about 6 minutes
tools/power-soak.sh --minutes 15   # watch it evolve for longer
tools/power-soak.sh --quick        # switching only, no waiting
```

It needs a running daemon. It closes the app if one is open and restarts
`pyren-daemon.service` with `sudo`, both on purpose: *what happens to the
profile when the app goes away* and *what survives the daemon restarting*
are the questions, and neither can be asked without doing it. It puts the
machine back in the mode it found it in on the way out, including on
Ctrl-C.

### The energy settings, and the fan modes Unlimited unlocks

A third suite answers a different question - not *which* profile a mode
selects, but whether the **envelope** it carries reaches the hardware:

```sh
cd daemon && cargo test --test energy_profiles
```

It lives in `daemon/tests/` rather than in a crate because the question
is cross-module, and the daemon is the only thing that can see all three
owners at once:

| module | what it owns |
|---|---|
| `pyren-power` | the package limits (PL1/PL2), the turbo knob, the profile |
| `pyren-fan` | `pwm1` and `pwm1_enable` - the fans |
| `pyren-overclock` | the GPU's offsets, and nothing else |

**Those three never call each other**, and half this file is about
keeping it that way: changing the power mode must not write to the fans,
changing the fan mode must not write to the envelope, and an overclock
request must not reach the CPU's power limits. The app presents Unlimited
as the mode that unlocks manual power limits *and* manual fan control,
but that grouping is the frontend's policy - the daemon will set a fan
curve in Eco perfectly happily, and the tests say so rather than
pretending otherwise.

Two deliberate limits on what it does:

- **No test applies an offset to a GPU.** The reference laptop has a real
  card and a consent on file, so an `apply` in a test would drive it -
  which is what that module's warning is about. Only the path that stops
  at the consent gate is exercised; the offsets themselves are covered by
  `pyren-overclock`'s own tests.
- **`manual` fan mode gets one test and no more.** It pins the fans at a
  speed nobody is watching. What matters is that it is accepted where
  `pwm1` exists and refused where it does not - one assertion each - and
  a fixture without `pwm1` (board 8D2F) covers the refusal.

To check the envelope on real hardware instead, `pyren-ctl` is enough:

```sh
pyren-ctl power tune --mode performance --pl1 45 --pl2 60
pyren-ctl power set performance
cat /sys/class/powercap/intel-rapl:0/constraint_0_power_limit_uw   # 44 W
pyren-ctl power tune --mode performance --pl1 77 --pl2 77          # put it back
```

The 45 that comes back as 44 is not a bug: limits are stored as a whole
percentage of this machine's own ceiling so that a restored config means
the same thing on different hardware, and one percent of 77 W is 0.77 W.

The two lifecycle answers, since they come up often:

| event | what happens to the profile |
|---|---|
| the app is closed, minimised or backgrounded | nothing. The app is a client; the mode belongs to the daemon, which is still running |
| the daemon restarts | the mode in memory is gone. It comes back only if `restoreModeOnStart` is on - otherwise the daemon reports whatever the firmware is actually set to, and changes nothing |

## Continuous integration

`.github/workflows/ci.yml` runs on every push to `main`, every pull
request, and on demand. Four jobs, so a failure names the half that broke:

| job | what it runs |
|---|---|
| `daemon` | `cargo test --workspace`, `cargo clippy --all-targets -- -D warnings` |
| `app` | `bun install --frozen-lockfile`, `bun run check`, `bun run build` |
| `tauri` | `cargo check --all-targets` on `app/src-tauri`, after installing WebKitGTK |
| `shell` | `sh -n tools/pyren-check.sh`, `sh -n tools/power-soak.sh` |

Two things worth knowing about it:

- The daemon job runs `check/tests/parity.rs`, which invokes
  `tools/pyren-check.sh` through `/bin/sh` — `dash` on the runner, rather
  than the `bash` or `zsh` it usually gets locally. That is the point of a
  POSIX script, and CI is the only place it is regularly checked.
- Nothing there can prove the socket's permissions *work*, only that the
  mode bits are right: the assertion that matters is "a second local user
  cannot connect", and CI has one user.
