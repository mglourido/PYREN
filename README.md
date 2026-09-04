# Pyren

A Tauri-based clone of HP's OMEN Gaming Hub for Linux, built as a
privileged daemon (Rust) plus an unprivileged desktop app (Tauri +
SvelteKit), so hardware-control modules can be ported in from separate
source projects — starting with fan control from
[`omen-fan-control`](https://github.com/arfelious/omen-fan-control). Their
Python is not vendored here (where those checkouts live is in
[`dev/README.md`](dev/README.md)); their **kernel driver** is, verbatim, in
[`driver/`](driver/README.md), because an installer that needs you to go
and clone something else first is not an installer.

Work still to do, and the findings behind the decisions taken so far, are
in [`dev/`](dev/README.md).

See [`docs/00-design-plan.md`](docs/00-design-plan.md) for the
architecture, [`docs/01-ipc-protocol.md`](docs/01-ipc-protocol.md) for the
wire format between the app and the daemon, and
[`docs/02-development.md`](docs/02-development.md) for how to build and
run everything, and [`docs/03-frontend.md`](docs/03-frontend.md) for the
frontend's structure and conventions.

`pyren-ctl status` is the fastest way to see what a running daemon
thinks the machine can do.

## Layout

```
daemon/     Rust workspace: pyren-daemon + pyren-ctl + pyren-check + module crates
app/        Tauri app: SvelteKit frontend + src-tauri shell
driver/     the patched hp-wmi kernel module, a verbatim copy of upstream's (C, GPL-2)
docs/       design plan + IPC protocol + development + frontend + RGB review
dev/        working notes: what is left to do, and what was learned
tools/      pyren-check.sh, the dependency-free fan self-test
```

## Status

- Daemon skeleton + Unix socket IPC: working. The socket is the trust
  boundary and is enforced as one — bound `0660` to the `pyren` group, so
  a local user who is not a member cannot reach a root daemon at all.
- Continuous integration: `.github/workflows/ci.yml` runs the daemon tests
  and clippy, `svelte-check` and the app build, `cargo check` on the Tauri
  shell, and a syntax check on the shell script.
- Config persistence: `pyren-config`, one JSON file per namespace with
  atomic writes, corrupt files preserved rather than overwritten, and
  version stamping. Shared by the daemon (`/etc/pyren/`) and the app
  (`~/.config/pyren/`).
- `system` module: machine identification (DMI/CPU/GPU, plus a
  compatibility verdict derived from what the hardware modules found they
  could actually do — never from a board list), and full live monitoring:
  CPU per core, memory, hwmon temperatures and fans, disks, network
  throughput, GPU (nvidia-smi and DRM sysfs) and the busiest processes.
  Generic Linux, so it works and is testable on any machine.
- `power` module: Eco/Balanced/Performance/Unlimited as real *profiles*,
  applied as three separable parts —

  1. **the laptop's own** firmware profile (ACPI `platform_profile`), the
     one that moves the EC's fan curve and internal power states;
  2. **the OS profile**, delegated to power-profiles-daemon rather than
     reimplemented, and optional: changing how the laptop behaves without
     changing what the desktop thinks is a reasonable thing to want;
  3. **the package power envelope** (PL1/PL2 and turbo), which ships
     *untouched*. Every laptop's internal profiles are its own, so this
     project invents no numbers; `pyren-ctl power tune` is how one that
     somebody measured gets recorded. Nothing is ever raised above what the
     firmware shipped — that would be overclocking, deliberately last.

  Plus a background supervisor: two systems, one per power source.
  Unplugging drops to Balanced and plugging in steps up to Performance,
  each then refining towards Eco or Performance as conditions hold.
  Unlimited is never chosen for you.
- `pyren-ctl`: a CLI over the same socket — `status`, `power set`,
  `power tune --pl1 35W`, `fan curve 40:20,80:100`, `rgb probe`, and
  `--json` on any of them. It exists mainly so a number someone measured on their own machine
  can be recorded without a slider.
- Fan-control self-test: `fan.diagnose`, the app's Hardware check page, a
  standalone `pyren-check` binary, and `tools/pyren-check.sh` — a
  dependency-free shell version to copy onto a machine where building isn't
  practical (kept in step by a parity test). Verifies what the running kernel
  actually supports instead of installing a patched driver — manual fan
  control is upstream in recent kernels, so on most machines there is
  nothing to install. Where the stock driver comes up *without* `pwm1`,
  though, that is the board missing from its tables, and the installer is
  the remedy rather than a downgrade.
- `installer` module: ports the driver/service installer as inspect → plan
  → apply, kept for boards the stock driver doesn't support and for the
  systemd unit. The driver it installs ships in `driver/`, and
  `installer.autodetect` works out what an install needs rather than asking
  for it — the board id from DMI, the right driver table from the driver's
  own source, the fan ceilings from the last calibration — so the app has
  one install button instead of a form. It has been **run for real**, on
  the test laptop: board 8D2F was missing from the stock driver's feature
  table, the patched module was built and installed, and `pwm1` appeared
  where the stock 7.2.2 driver produced none — see
  `dev/FINDINGS.md` §"The patched driver works on 8D2F".
- `fan` module: status, the self-test, and the write path — `setMode`
  (auto/max/manual/curve), `setCurve`, a control loop that follows a curve
  with hysteresis and temperature smoothing, and `fan.json` persistence.
  What a machine can actually do is reported as `capabilities` and enforced:
  `auto` and `max` need only `pwm1_enable`, while a *speed* needs `pwm1`,
  which the running driver exposes only for boards in its feature table. On
  the test laptop (board 8D2F) that means max and auto are available and a
  percentage is not — the module says so instead of failing silently. Max
  and auto are verified against the hardware (fans go to ~3900 rpm and come
  back). `fan.calibrate` measures what full speed actually is on a machine
  - the number the curve's hysteresis wants and otherwise has to guess at -
  and needs only mode switching, so it runs on boards that cannot be given
  a percentage. The **fan cleaner** is ported (`fan.cleanerStatus`,
  `startCleaning`, `stopCleaning`): dust removal by spinning the fans
  backwards, over `/proc/acpi/call` rather than the kernel driver, in both
  the modern ("CleanCreek") and legacy firmware dialects. Reverse spin is
  cooling switched *off* for as long as it runs, so the timeout is enforced
  three separate ways, a cycle found still running at daemon startup is
  ended, and every failure path ramps the fans back down rather than
  releasing them abruptly. **It has never been run against firmware that
  has the feature** — same reason as the lightbar below — so what is tested
  is the buffers, the replies, the capability decoding and the guards.
- `rgb` module: the OMEN lighting, which is really *two unrelated things* —
  per-key RGB over USB HID (`0d62:54bf`) and a 4-zone bottom light strip
  over ACPI-WMI — that share no transport, no privileges and no detection.
  **Which one a laptop has is not decided by its model name**, so both are
  probed and `rgb.getCapabilities` reports what was found; there is no
  board list here either. The lightbar is ported (`setZones`, `setStatic`,
  `off`, and a `readZones` that really does ask the firmware) with its
  144-byte payload unit-tested field by field, and three bugs in the source
  project fixed rather than carried over. **It has never been run against a
  light strip**: `/proc/acpi/call` needs the `acpi_call` kernel module,
  which the test laptop does not have installed — so the module says which
  of the three ways it is unavailable a machine is in, rather than
  "no lighting". The per-key path is detected and deliberately not driven.
  `/proc/acpi/call` is a single global interface with no locking, so every
  use goes through one process-wide lock in `pyren-core` — shared with the
  fan cleaner, which speaks the same `SECU` buffer protocol through the
  same file.
- App: a full OMEN Gaming Hub-style frontend — home dashboard, system vitals
  (basic + advanced views), performance control (power modes, fan
  toggle/curve, power limits), GPU overclocking, fan cleaning, lighting,
  graphics switcher, network booster, key mapping, plus settings, drivers
  and help pages. Fan and power writes reach the daemon, and the pages hide what
  this machine's driver cannot do rather than offering controls that
  silently fail. GPU switching, network booster and key mapping are still
  UI-only, and so is the lighting page — the `rgb` module exists but is not
  wired to it, because a UI built on a backend nobody has confirmed against
  hardware is a UI that lies convincingly. Bilingual (en/es) with a drop-in translation system,
  and it falls back to simulated data when the daemon isn't reachable, so
  the UI is usable without root.

## Running in development

Short version: run `cargo run -p pyren-daemon` in `daemon/`, then
`bun run tauri dev` in `app/`. Full prerequisites, first-build notes, and a
Wayland workaround you'll likely need are in
[`docs/02-development.md`](docs/02-development.md).

## Production deployment (not set up yet)

The daemon is meant to run as a systemd service as root, with
`PYREN_SOCKET=/run/pyren/daemon.sock`; the app runs as a normal
desktop user and connects to that socket, which requires being in the
`pyren` group:

```sh
sudo groupadd -f pyren && sudo usermod -aG pyren "$USER"   # then log out and back in
```

`installer.apply` does the `groupadd` itself; the `usermod` is left to the
user, since which accounts may control the machine is not the installer's
decision. No packaging exists yet — see the roadmap in
`docs/00-design-plan.md`.

### Coming from a pre-rename install

This was called Omen Hub until 2026-09-02, and the rename moved everything
it owns. A machine that ran the old build has three things left behind, and
nothing migrates them automatically:

```sh
sudo mv /etc/omen-hub /etc/pyren                # daemon settings
mv ~/.config/omen-hub ~/.config/pyren           # app settings
sudo groupmod -n pyren omen-hub                 # the socket's group
```

`OMEN_HUB_SOCKET` is now `PYREN_SOCKET`, and the systemd unit is
`pyren-daemon.service` — the old one should be disabled before the new one
is installed, or two daemons will fight over the same hardware.

## License

GPL-3.0-or-later, for the whole repository — daemon, app and tools. See
[`LICENSE`](LICENSE).

    Copyright (C) 2026 Mateo González

This is not a free choice: the `fan` and `installer` modules are ports of
[`omen-fan-control`](https://github.com/arfelious/omen-fan-control), which
is GPL-3.0, and the driver they patch is GPL-2 kernel code. The frontend
was original work and previously declared MIT in `app/package.json`; it now
matches the rest, because the app and the daemon ship as one program.
