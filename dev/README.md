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
├── check/              pyren-check: standalone fan self-test CLI
├── ctl/                pyren-ctl: shell client for a running daemon
└── crates/
    ├── core/           Module trait, Registry, wire types, socket server + client
    ├── config/         on-disk settings (atomic writes, versioning)
    ├── system/         machine identity + generic Linux monitoring
    ├── power/          power profiles + the auto-switch supervisor
    ├── fan/            fan status, the write path, the self-test
    └── installer/      driver/service installer (inspect → plan → apply)
app/                    Tauri app: SvelteKit frontend + src-tauri shell
tools/pyren-check.sh     dependency-free shell twin of pyren-check
docs/                   design, IPC protocol, development, frontend, RGB review
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
(the behavioural spec) and
`src/omen_fan_control/data/driver/hp-wmi-omen/hp-wmi.c` (the driver itself,
which is plain upstream — the patching happens at install time, so the
`.orig` beside it is byte-identical).

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

- **Monitoring**: real, on any machine. CPU per core, memory, hwmon
  temperatures and fans, disks, network, GPU, top processes.
- **Machine identification**, and a verdict about what this machine can be
  *told to do*, measured rather than looked up in a board list.
- **Power profiles**: the laptop's own firmware profile and the OS one
  (delegated to power-profiles-daemon) as separate switches, plus a package
  power envelope that ships untouched until someone measures their machine.
  With the auto-switch supervisor: unplugging drops to Balanced, plugging
  in steps up to Performance, each refining from there.
- **Fan self-test**: three front ends (daemon method, app page, CLI + shell
  script), kept in step by a parity test.
- **Fan control**: mode switching (max and auto measured on the laptop,
  ~2000 → ~3900 rpm and back), a curve followed on the daemon's own thread,
  hysteresis, calibration (`fan.calibrate` — max, watch, restore, which
  needs only mode switching and so runs on this board), and settings that
  survive a restart — as far as the hardware allows, which it reports
  rather than guesses.
- **Frontend**: the whole Pyren surface, bilingual, with settings on
  disk. Fan and power controls reach the daemon; the pages hide what this
  machine cannot do.
- **`pyren-ctl`**: `status`, `power set|tune|auto|os-profile`,
  `fan set|curve|diagnose|calibrate`, `--json` on anything.
- **A socket other local users cannot open**: `0660`, group `pyren`
  (verified against a root daemon - a process outside the group gets
  `EACCES`).

## What does not work yet

- **Setting a fan *percentage* on this laptop.** The write path is built and
  tested; the running kernel exposes no `pwm1`, so `manual` and `curve` are
  refused here while `auto` and `max` are not. Installing the patched
  driver should change that — see `TODO.md` §1.1.
- **Lighting, GPU switching, network booster, key mapping**: the UI is
  complete and drives local state only. No daemon module behind any of them.
- **The installer's execution path** has never been run. It is `TODO.md`
  §1.1 and the single most valuable thing left, because it is also the test
  of whether this board can be given a fan percentage at all.
- **Overclocking**, deliberately: it is the only feature that would leave
  the envelope the firmware shipped, and it goes last.

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

A machine that ran the old build keeps its old files; the three commands
that move them are in the README's deployment section.

## Verifying a change

```sh
cd daemon && cargo test && cargo clippy --all-targets   # 178 tests, 0 warnings
cd app && bun run check && bun run build
cd app/src-tauri && cargo check
sh -n tools/pyren-check.sh
```

`.github/workflows/ci.yml` runs all of these on every push and pull
request, in four jobs so a failure says which half broke. Running them
locally first is still faster than waiting for the badge.
