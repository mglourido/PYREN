# dev/ — working notes

Notes for whoever picks this up next, including future me. Three files:

| file | what it holds |
|---|---|
| [`TODO.md`](TODO.md) | everything still to do, in priority order, with what blocks each item |
| [`FINDINGS.md`](FINDINGS.md) | facts that cost real effort to establish — don't re-derive them |
| this file | where things are, and how to get running |

`docs/` is the *documentation*: how the thing works, for someone using or
extending it. `dev/` is the *work queue*: what's missing and what to be
careful of. If something here becomes settled and permanent, it belongs in
`docs/` instead.

These notes are in English to match the rest of the repository, even though
the project's conversations are in Spanish.

## Where things are

```
daemon/                 Rust workspace (the daemon and its two CLIs)
├── daemon/             pyren-daemon: loads modules, serves the socket
├── check/              pyren-check: standalone compatibility check CLI
├── ctl/                pyren-ctl: shell client for a running daemon
└── crates/
    ├── core/           Module trait, Registry, wire types, socket server + client
    ├── config/         on-disk settings (atomic writes, versioning)
    ├── system/         machine identity + generic Linux monitoring
    ├── power/          power profiles + the auto-switch supervisor
    ├── fan/            fan status, the write path, the self-test
    ├── rgb/            lighting: probes both paths, drives the 4-zone keyboard
    ├── gpu/            MUX-mode switching (hp-wmi's own gpu_mux_mode)
    ├── network/        system-wide qdisc (cake/fq_codel) on the default route
    ├── keymap/         evdev-level key remapping (/dev/uinput)
    ├── overclock/      GPU clock offsets/lock, one consent gate of its own
    └── installer/      driver/service installer (inspect → plan → apply)
app/                    Tauri app: SvelteKit frontend + src-tauri shell
tools/pyren-check.sh     dependency-free shell twin of pyren-check
driver/                 the patched hp-wmi module, verbatim from upstream (never edited here)
docs/                   architecture, IPC protocol, development, frontend
```

## The source projects being ported from

Not in this repository and not on the developer's disk — they live on a USB
stick, which is worth writing down because every path in these notes that
starts `../omen-fan-control-main` actually resolves to:

```
/run/media/paraguayo33/SAMSUNG USB/omen-fan-control-main (1)/    # docs/ + the Python source + the driver
/run/media/paraguayo33/SAMSUNG USB/omen-rgb-linux-main/          # the RGB project
```

The one to read first is `docs/04-fan-control-logic.md` in the fan project
(the behavioural spec). The driver itself no longer needs the stick: it is
copied into this repository at `driver/`, with its provenance in
`driver/README.md`. It is plain upstream — the patching happens at install
time, on a copy staged under `/usr/src`, so `hp-wmi.c` and the `.orig`
beside it stay byte-identical here.

## Running everything

```sh
# terminal 1
cd daemon && cargo run -p pyren-daemon

# terminal 2
cd app && bun run tauri dev
```

Full prerequisites and the Wayland workaround are in
[`docs/02-development.md`](../docs/02-development.md).

To see what the daemon thinks without opening the app:

```sh
cd daemon && cargo run -q -p pyren-ctl -- status
```

Reaching a daemon that is running as **root** means being in its group:
`sudo groupadd -f pyren && sudo usermod -aG pyren $USER`, then a new
login (or `newgrp pyren` for one shell).

## What actually works today

Every hardware module is built, wired end to end (daemon ↔ app ↔
`pyren-ctl`), and confirmed against this laptop:

- **Monitoring**: CPU per core, memory, hwmon temperatures and fans,
  disks, network, GPU, top processes — real on any machine.
- **Machine identification**, and a verdict about what this machine can be
  *told to do*, measured rather than looked up in a board list.
- **The patched driver, installed.** `pwm1`/`pwm2` exist on this board
  because of it. The installer's plan → apply flow has been run
  repeatedly, including a full reinstall with a fan ceiling this
  installer measured itself, and it correctly recognises a strategy
  switch (DKMS ↔ kernel hooks) and retires the one it leaves behind. A
  driver install or restore makes the running daemon re-read its own
  hardware (`FanModule::rediscover`) — nothing to restart by hand.
- **Fan control, fully**: mode switching (`auto`/`max` and now
  `manual`/`curve`, since `pwm1` exists), a curve followed on the
  daemon's own thread, hysteresis, calibration (`fan.calibrate` — max,
  watch, restore, settle early). A measurement it takes is pinned into
  the driver itself via a module parameter that outranks the firmware's
  own claim — see `FINDINGS.md` §"The patched fan ceiling was never
  reaching the driver" for why that needed to exist at all.
- **Fan self-test**: three front ends (daemon method, app page, CLI +
  shell script), kept in step by a parity test.
- **Power profiles**: the laptop's own firmware profile and the OS one
  (delegated to power-profiles-daemon) as separate switches, plus a
  package power envelope that ships untouched until someone measures
  their machine. The auto-switch supervisor: unplugging drops to
  Balanced, plugging in steps up to Performance, each refining from
  there, plus a thermal rule that outranks both.
- **Lighting**: both ACPI dialects (`fourZone`, `kernelZones`) confirmed
  against the real light strip, zone 4's read fixed by the `kernelZones`
  path, dialect auto-picked or pinned by hand.
- **GPU switching**: `gpu_mux_mode` written and read back correctly
  (`hybrid` ↔ `discrete`) — the one untested part is watching a reboot
  actually swap the driving card, see `TODO.md`.
- **Key mapping**: an evdev-level remapper (`/dev/uinput`), built and
  wired end to end — not yet run against real hardware, see `TODO.md`.
- **Network booster**: the one honest half (system-wide `cake`/`fq_codel`
  via the default-route interface) confirmed on hardware; per-process
  prioritisation was scoped out on purpose, see `TODO.md` §3.
- **GPU overclocking**: core and memory offsets applied and reverted on a
  real GPU through NVML (`libnvidia-ml`), which needs no X and no
  `Coolbits`; the clock lock (`nvidia-smi --lock-gpu-clocks`) and its
  revert-on-lapse timer are proven the same way. The only unexercised
  path is the `nvidia-settings` fallback for drivers too old for NVML's
  offset support. See `TEST.md`.
- **Frontend**: the whole Pyren surface, bilingual, with settings on
  disk, and a live progress overlay for driver actions (one segment per
  step, driven by the daemon's own `installer.progress` events).
- **`pyren-ctl`**: `status`, `power set|tune|auto|os-profile`,
  `fan set|curve|diagnose|calibrate`, `rgb`, `gpu`, `network`, `keymap`,
  `oc get|probe|consent|set|confirm`, `--json` on anything.
- **A socket other local users cannot open**: `0660`, group `pyren`
  (verified against a root daemon - a process outside the group gets
  `EACCES`).

## What is still open

See `TODO.md` for the current, short list — mainly things that need
different hardware to finish confirming (AMD Overdrive, keymap against a
spare keyboard, the GPU MUX reboot swap) or a deliberate decision
(raising CPU power limits above stock).

## The rename

The project was **Omen Hub** until 2026-09-02 and is now **Pyren**, which
moved every name it owns: crates and binaries (`pyren-daemon`,
`pyren-ctl`, `pyren-check`), the socket group, `/etc/pyren`,
`~/.config/pyren`, `PYREN_SOCKET`, and `tools/pyren-check.sh`.

What deliberately did **not** move: `hp-wmi`, `omen-fan-control`,
`omen-rgb-linux`, `omen_thermal_profile_boards`, `OMEN_CPU_MAX_RPM` and
"OMEN Gaming Hub". Those are other people's names — the kernel's, the
upstream projects', HP's — and renaming them would be renaming things this
project does not own.

A machine that ran the old build keeps its old files; nothing migrates
them automatically:

```sh
sudo mv /etc/omen-hub /etc/pyren                # daemon settings
mv ~/.config/omen-hub ~/.config/pyren           # app settings
sudo groupmod -n pyren omen-hub                 # the socket's group
```

`OMEN_HUB_SOCKET` is now `PYREN_SOCKET`; disable the old
`omen-hub-daemon.service` before installing `pyren-daemon.service`, or two
daemons fight over the same hardware.

## Verifying a change

```sh
cd daemon && cargo test && cargo clippy --all-targets   # 456 tests, 0 warnings
cd app && bun run check && bun run build
cd app/src-tauri && cargo check
sh -n tools/pyren-check.sh
```

`.github/workflows/ci.yml` runs all of these on every push and pull
request, in four jobs so a failure says which half broke. Running them
locally first is still faster than waiting for the badge.
