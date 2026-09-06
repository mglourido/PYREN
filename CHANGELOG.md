# Changelog

All notable changes to Pyren are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

While Pyren is pre-1.0, minor versions may still make breaking changes to
the IPC protocol and on-disk config.

## [Unreleased]

### Added

- **One fan curve per power profile.** Eco, Balanced, Performance and
  Unlimited each keep their own curve, and the one driving the fans follows
  the machine — including when the performance key, the OSD or the daemon's
  own supervisor moves the mode with no app open. The performance page gets
  a profile selector above the curve editor, mirroring the one the power
  limits already had, and says whether the curve on screen is the one
  running. `fan.setCurve` takes an optional `profile`; `pyren-ctl fan curve`
  takes `--profile`. Existing curves are copied to every profile on first
  use, so nothing tuned is lost.
  - The fan module learns the profile by **subscribing to `power.mode` on
    the event bus**, not by either module calling the other: `power` already
    announced without knowing who listened, and `EventBus::subscribe` is the
    listening half. The profile is an opaque string inside `pyren-fan`, so
    it still has no idea what a power mode is.

- **`fan.probeSpeedControl`** (`pyren-ctl fan probe-speed`, and a button on
  the performance page): holds the fans at a speed they are not at and
  watches whether they follow. It is the only way to tell a driver that
  honours `pwm1` from one that accepts the value and ignores it — board
  `8D2F` is the second kind, and every check before this passed on it. The
  answer is remembered in `fan.json` (`speedControl`); a machine that
  ignores a commanded speed reports `capabilities.setSpeed: false` from
  then on, so clients stop offering a curve nothing follows, and
  `fan.setMode` refuses `manual`/`curve` with a reason that is explicitly
  *not* "install a driver".

### Fixed

- **`fan.diagnose`'s write check could not fail.** It wrote back the value
  already in `pwm1` and compared — but on a driver whose `pwm1` reports the
  *measured* fan speed rather than the setpoint, that is a tautology. It
  reported `wrote and read back pwm1 = 62 without changing fan speed` as a
  pass and concluded `fullControl` about hardware that has never obeyed a
  commanded speed. It now writes a value the channel is not already at, and
  a mismatch is reported as the signature of that class of board. The same
  fix is in `tools/pyren-check.sh`.
- The self-test gained a `pwm-effect` check, fed by the probe above, so
  `fullControl` now means the fans were watched to move rather than that a
  file exists.

### Changed

- **Fan control is no longer Unlimited-only.** The `manual` and `curve`
  fan modes and the editable fan curve are now offered in every power
  mode (Eco / Balanced / Performance / Unlimited) on machines whose
  driver can be told a fan speed. The daemon already kept one curve and
  applied it regardless of the power mode; only the app's UI had tied the
  two together. Manual *power* limits stay grouped with Performance and
  Unlimited.

## [0.1.0] — 2026-09-05

First public release. Everything below is built, wired end to end
(daemon ↔ app ↔ `pyren-ctl`) and confirmed against the development
laptop; `TEST.md` is the feature-by-feature record of what has actually
been exercised on hardware and what has not.

### Added

- **Daemon** (`pyren-daemon`): a privileged Rust host process that loads
  the hardware modules and serves them over a `0660`, `pyren`-group Unix
  socket — a local user outside the group cannot reach a root daemon.
  Installs its own systemd unit with `pyren-daemon --install-service`.
- **Desktop app** (`pyren`): the full OMEN Gaming Hub-style surface —
  dashboard, system vitals (basic + advanced), performance control, GPU
  overclocking, fan cleaning, lighting, graphics switcher, network
  booster, key mapping, settings, drivers and help. Bilingual (en/es),
  settings persisted to disk, live progress overlay for driver actions,
  simulated readings when no daemon is reachable.
- **On-screen display** (`pyren-osd`): a GTK4 layer-shell widget the
  performance key puts on screen; started by the app, or as a user
  service.
- **`pyren-ctl`**: shell client for a running daemon — `status`,
  `power set|tune|auto|os-profile`, `fan set|curve|diagnose|calibrate`,
  `rgb`, `gpu`, `network`, `keymap`, `oc`, `--json` on anything.
- **`pyren-check`**: standalone compatibility probe (no daemon, socket or
  GUI), with a dependency-free shell twin in `tools/pyren-check.sh`.
- **Monitoring**: CPU per core, memory, hwmon temperatures and fans,
  disks, network, GPU (NVIDIA via `nvidia-smi`, DRM sysfs), top
  processes. Generic Linux — works on any machine.
- **Machine identification and a compatibility verdict** derived from
  what the hardware modules actually accept, never from a board list.
- **Power profiles**: Eco / Balanced / Performance / Unlimited as the
  firmware profile and the OS profile (via power-profiles-daemon) as
  separate switches, plus a package power envelope that ships untouched
  until someone measures their machine. Auto-switch supervisor for
  battery / load / heat.
- **Fan control**: `auto` / `max` / `manual` / `curve`, a curve followed
  on the daemon's thread with hysteresis, calibration, and a self-test
  with three front ends kept in step by a parity test. The reverse-spin
  fan cleaner is ported (never yet run against firmware that has it).
- **Driver installer**: vendors the patched `hp-wmi` tree, works out what
  an install needs (board id, driver table, measured fan ceiling), and
  drives DKMS or the distribution's kernel hook — inspect → plan → apply.
- **Lighting**: the 4-zone ACPI lightbar, both dialects (`fourZone`,
  `kernelZones`), auto-picked or pinned.
- **GPU switching**: `gpu_mux_mode` (`hybrid` ↔ `discrete`), written and
  read back.
- **GPU overclocking**: core and memory offsets and a clock lock through
  NVML (no X, no `Coolbits`), behind a consent gate and a
  revert-on-lapse timer; reverts on a reported GPU fault.
- **Network booster**: system-wide `cake` / `fq_codel` on the default
  route.
- **Key mapping**: an evdev-level remapper over `/dev/uinput` (built and
  wired, not yet run against real hardware).
- **Packaging**: `tools/release.sh` builds an optimized, self-contained
  `pyren-<version>-x86_64-linux.tar.gz`; `install/install.sh` installs it
  system-wide, sets up both systemd units and the `pyren` group. See
  `install/INSTALL.md`.

### Known limitations

See `dev/TODO.md` and `TEST.md`. In brief: key mapping and the GPU MUX
reboot swap have not been watched on hardware; the fan cleaner and the
per-key USB RGB path are unproven / unported; no power profile raises a
limit above what the firmware shipped.

[Unreleased]: https://github.com/mglourido/PYREN/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/mglourido/PYREN/releases/tag/v0.1.0
