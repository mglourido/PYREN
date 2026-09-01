# Omen Hub

A Tauri-based clone of HP's OMEN Gaming Hub for Linux, built as a
privileged daemon (Rust) plus an unprivileged desktop app (Tauri +
SvelteKit), so hardware-control modules can be ported in from separate
source projects — starting with fan control from
[`omen-fan-control`](../omen-fan-control-main).

See [`docs/00-design-plan.md`](docs/00-design-plan.md) for the
architecture, [`docs/01-ipc-protocol.md`](docs/01-ipc-protocol.md) for the
wire format between the app and the daemon, and
[`docs/02-development.md`](docs/02-development.md) for how to build and
run everything, and [`docs/03-frontend.md`](docs/03-frontend.md) for the
frontend's structure and conventions.

## Layout

```
daemon/     Rust workspace: omen-hub-daemon (bin) + omen-hub-core + omen-hub-fan
app/        Tauri app: SvelteKit frontend + src-tauri shell
docs/       design plan + IPC protocol + development + frontend guide
```

## Status

- Daemon skeleton + Unix socket IPC: working.
- Config persistence: `omen-hub-config`, one JSON file per namespace with
  atomic writes, corrupt files preserved rather than overwritten, and
  version stamping. Shared by the daemon (`/etc/omen-hub/`) and the app
  (`~/.config/omen-hub/`).
- `system` module: machine identification (DMI/CPU/GPU + an OMEN
  compatibility verdict) and full live monitoring — CPU per core, memory,
  hwmon temperatures and fans, disks, network throughput, GPU (nvidia-smi
  and DRM sysfs) and the busiest processes. Generic Linux, so it works and
  is testable on any machine.
- `power` module: Eco/Balanced/Performance/Unlimited switching through the
  ACPI platform profile, power-profiles-daemon or the CPU energy-performance
  hint, plus a background supervisor that switches modes on its own (Eco on
  battery, Performance under sustained load) with hysteresis and a manual
  override.
- `installer` module: ports the driver/service installer as inspect → plan
  → apply. Detection and planning are verified; the execution path is
  written but has never been run, because that needs an HP laptop.
- `fan` module: read-only status (`getStatus`) implemented and verified
  end-to-end; writing to hardware (`setMode`/`setCurve`/fan cleaner) not
  ported yet.
- App: full OMEN-Hub-style frontend — home dashboard, system vitals
  (basic + advanced views), performance control (power modes, fan
  toggle/curve, power limits), GPU overclocking, lighting, graphics
  switcher, network booster, key mapping, plus settings, drivers and help
  pages. Bilingual (en/es) with a drop-in translation system. Falls back to
  simulated data when the daemon isn't reachable, so the UI is usable
  without root. Hardware *writes* are wired in the UI but no-ops until the
  daemon implements them.

## Running in development

Short version: run `cargo run -p omen-hub-daemon` in `daemon/`, then
`bun run tauri dev` in `app/`. Full prerequisites, first-build notes, and a
Wayland workaround you'll likely need are in
[`docs/02-development.md`](docs/02-development.md).

## Production deployment (not set up yet)

The daemon is meant to run as a systemd service as root, with
`OMEN_HUB_SOCKET=/run/omen-hub/daemon.sock`; the app runs as a normal
desktop user and connects to that socket. No systemd unit, installer, or
packaging exists yet — see the roadmap in `docs/00-design-plan.md`.
