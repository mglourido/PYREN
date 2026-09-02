# Omen Hub

A Tauri-based clone of HP's OMEN Gaming Hub for Linux, built as a
privileged daemon (Rust) plus an unprivileged desktop app (Tauri +
SvelteKit), so hardware-control modules can be ported in from separate
source projects — starting with fan control from
[`omen-fan-control`](../omen-fan-control-main).

Work still to do, and the findings behind the decisions taken so far, are
in [`dev/`](dev/README.md).

See [`docs/00-design-plan.md`](docs/00-design-plan.md) for the
architecture, [`docs/01-ipc-protocol.md`](docs/01-ipc-protocol.md) for the
wire format between the app and the daemon, and
[`docs/02-development.md`](docs/02-development.md) for how to build and
run everything, and [`docs/03-frontend.md`](docs/03-frontend.md) for the
frontend's structure and conventions.

## Layout

```
daemon/     Rust workspace: omen-hub-daemon + omen-hub-check (CLI) + module crates
app/        Tauri app: SvelteKit frontend + src-tauri shell
docs/       design plan + IPC protocol + development + frontend guide
dev/        working notes: what is left to do, and what was learned
tools/      omen-check.sh, the dependency-free fan self-test
```

## Status

- Daemon skeleton + Unix socket IPC: working. The socket is the trust
  boundary and is enforced as one — bound `0660` to the `omen-hub` group, so
  a local user who is not a member cannot reach a root daemon at all.
- Continuous integration: `.github/workflows/ci.yml` runs the daemon tests
  and clippy, `svelte-check` and the app build, `cargo check` on the Tauri
  shell, and a syntax check on the shell script.
- Config persistence: `omen-hub-config`, one JSON file per namespace with
  atomic writes, corrupt files preserved rather than overwritten, and
  version stamping. Shared by the daemon (`/etc/omen-hub/`) and the app
  (`~/.config/omen-hub/`).
- `system` module: machine identification (DMI/CPU/GPU, plus a
  compatibility verdict derived from what the hardware modules found they
  could actually do — never from a board list) and full live monitoring — CPU per core, memory,
  hwmon temperatures and fans, disks, network throughput, GPU (nvidia-smi
  and DRM sysfs) and the busiest processes. Generic Linux, so it works and
  is testable on any machine.
- `power` module: Eco/Balanced/Performance/Unlimited as real *profiles* —
  each sets the OS power profile (ACPI platform profile,
  power-profiles-daemon or the CPU energy-performance hint) **and** the
  package power envelope: PL1/PL2 as a percentage of the machine's own
  stock limits, plus turbo. The envelope is the half the fans feel, and on
  a machine with no firmware platform profile it is the whole profile.
  Nothing is ever raised above stock — that would be overclocking, which is
  deliberately not built yet — and no envelope numbers ship by default,
  because every laptop's internal profiles are its own and guessing them
  would be worse than doing nothing. Plus a background supervisor: two
  systems, one per power source, where unplugging drops to Balanced and
  plugging in steps up to Performance, each then refining towards Eco or
  Performance as conditions hold. Unlimited is never chosen for you.
- Fan-control self-test: `fan.diagnose`, the app's Hardware check page, a
  standalone `omen-hub-check` binary, and `tools/omen-check.sh` — a
  dependency-free shell version to copy onto a machine where building isn't
  practical (kept in step by a parity test). Verifies what the running kernel
  actually supports instead of installing a patched driver — manual fan
  control is upstream in recent kernels, so on most machines there is
  nothing to install.
- `installer` module: ports the driver/service installer as inspect → plan
  → apply, kept for boards the stock driver doesn't support and for the
  systemd unit. Detection and planning are verified; the driver execution
  path is written but has never been run, because that needs an HP laptop.
- `fan` module: status, the self-test, and the write path — `setMode`
  (auto/max/manual/curve), `setCurve`, a control loop that follows a curve
  with hysteresis and temperature smoothing, and `fan.json` persistence.
  What a machine can actually do is reported as `capabilities` and enforced:
  `auto` and `max` need only `pwm1_enable`, while a *speed* needs `pwm1`,
  which the running driver exposes only for boards in its feature table. On
  the test laptop (board 8D2F) that means max and auto are available and a
  percentage is not — the module says so instead of failing silently. Max
  and auto are verified against the hardware (fans go to ~3900 rpm and come
  back); the fan cleaner and calibration are not ported.
- App: full OMEN-Hub-style frontend — home dashboard, system vitals
  (basic + advanced views), performance control (power modes, fan
  toggle/curve, power limits), GPU overclocking, lighting, graphics
  switcher, network booster, key mapping, plus settings, drivers and help
  pages. The fan controls are wired to the daemon and hide what this
  machine's driver cannot do. Bilingual (en/es) with a drop-in translation system. Falls back to
  simulated data when the daemon isn't reachable, so the UI is usable
  without root. Fan and power writes reach the daemon; lighting, GPU
  switching, network booster and key mapping are still UI-only.

## Running in development

Short version: run `cargo run -p omen-hub-daemon` in `daemon/`, then
`bun run tauri dev` in `app/`. Full prerequisites, first-build notes, and a
Wayland workaround you'll likely need are in
[`docs/02-development.md`](docs/02-development.md).

## Production deployment (not set up yet)

The daemon is meant to run as a systemd service as root, with
`OMEN_HUB_SOCKET=/run/omen-hub/daemon.sock`; the app runs as a normal
desktop user and connects to that socket, which requires being in the
`omen-hub` group:

```sh
sudo groupadd -f omen-hub && sudo usermod -aG omen-hub "$USER"   # then log out and back in
```

`installer.apply` does the `groupadd` itself; the `usermod` is left to the
user, since which accounts may control the machine is not the installer's
decision. No packaging exists yet — see the roadmap in
`docs/00-design-plan.md`.

## License

GPL-3.0-or-later, for the whole repository — daemon, app and tools. See
[`LICENSE`](LICENSE).

    Copyright (C) 2026 Mateo González

This is not a free choice: the `fan` and `installer` modules are ports of
[`omen-fan-control`](https://github.com/arfelious/omen-fan-control), which
is GPL-3.0, and the driver they patch is GPL-2 kernel code. The frontend
was original work and previously declared MIT in `app/package.json`; it now
matches the rest, because the app and the daemon ship as one program.
