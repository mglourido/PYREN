# Changelog

All notable changes to Pyren are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

While Pyren is pre-1.0, minor versions may still make breaking changes to
the IPC protocol and on-disk config.

## [Unreleased]

### Added

- **The network page knows which application is using the connection.**
  A table of traffic per process, each with a rule: *Block* cuts it off
  from the network, *High* and *Low* move what it sends ahead of or behind
  everything else. Rules are by process name, survive restarts
  (`/etc/pyren/network.json`) and apply to every process of that name.
  Behind it are a few eBPF programs on the root cgroup that note which
  process opens each socket (closes #18) - no process is moved to another
  cgroup, and they detach by themselves if the daemon dies. Priority only
  reorders uploads and only while *Automatic* is running `cake`; a
  download can be blocked but not slowed. New `network.getProcesses` and
  `network.setRule`, `perProcess` in `network.getStatus`, and `pyren-ctl
  network procs` / `network rule <name> <action>`. Needs cgroup v2 and a
  root daemon; elsewhere the page says why instead.
- **The network mode and rules are remembered, per user.** *Automatic* used
  to be forgotten at every restart. It is now kept in `network.json` and
  put back once there is a default route to put it on - at boot that is
  some seconds after the daemon starts - and again if the route moves to
  another interface. Like the fans and the lighting, the file is each
  user's own: switching user switches mode and rules, and the daemon
  removes the queue it placed on its way out so nobody inherits the last
  person's.

- **The daemon's settings follow whoever is logged in.** One daemon serves
  the whole machine, and until now whoever changed its settings last
  changed them for everyone. It now keeps a copy per user
  (`/etc/pyren/users/<uid>/`) and watches the active session: when another
  of Pyren's users - a member of the `pyren` group - logs in or is switched
  to, it restarts itself with their fans, power, lighting, keys and
  overclock; a user with none of their own carries on with the ones in use.
  A GPU overclock is put back to stock when the machine changes hands and
  returns only through its own `restoreOnStart`.
  Nobody logged in means the most recently used ones, as before. What
  calibration measured about the fans stays the machine's. New `users`
  module (`users.getStatus`, the `users.changed` event), a Users panel in
  Settings, and `pyren-ctl users get`.
- **The daemon can stand down for people who do not use Pyren.**
  `users.setStandDownForOthers` (Settings > Users, `pyren-ctl users
  stand-down on`): while somebody who is not in the `pyren` group is the
  active user, the daemon lets go of the fans, the power override and any
  overclock and waits (the keyboard stays lit in its static colours; only
  an animated effect stops), so they are not left with another user's fan curve.
  Off by default.

- **The GPU fan can have a curve and a speed of its own.** Where the
  driver has a `pwm2`, `fan.setGpuFan { separate, curve, profile,
  manualPwm }` gives the second fan its own order: a curve per power
  profile that follows the GPU's temperature (the CPU's while the card is
  asleep) and its own manual speed. Off by default, which is one curve for
  both fans as before, and an undrawn GPU curve follows the CPU fan's.
  The guards, the floor and the hysteresis are unchanged - they are given
  the faster of the two orders. `fan.getStatus` reports it under `gpuFan`.
  Performance control gets a "One curve for both / GPU fan apart" switch.
- **A hardware check for it: `fan-split`.** With writes enabled,
  `fan.diagnose` tells one fan slow and the other fast, swaps them, and
  watches both tachometers. The answer is stored as `splitControl`; on a
  board that runs both fans together the setting is hidden and one curve
  drives everything. Board 8D2F passes: 3000 / 4500 rpm, then 4500 / 3000.

- **Pyren reads the ceiling the driver is working with, and says when it
  is not the measured one.** The vendored driver publishes it as
  `fan1_max` / `fan2_max`; `fan.getStatus` now reports it as
  `driverFan1MaxRpm` / `driverFan2MaxRpm`, with `driverCeilingLow` (no
  calibration, and a ceiling under 5000 rpm - usually the firmware fan
  table's fastest entry rather than the fans' limit) and `ceilingPin`
  (`applied`, `pending`, `notWritten` or `unsupported`: whether the loaded
  driver is using what calibration measured, and if not, whether its next
  load will). The Drivers page shows a notice for
  each, above the calibration check. omen-fan-control answers the same
  problem by refusing to install uncalibrated; Pyren cannot bake a wrong
  ceiling into the module, so it only warns. The driver's figure also
  stands in for a missing calibration in `fan.probeSpeedControl`, where
  it decides which way to aim and fills in `expectedRpm`. The curve's
  hysteresis and the thermal checker deliberately keep to measured
  values: a too-low ceiling would let the checker take slow fans for
  fans at full speed.
- **Pyren says when the installed driver is older than the one it ships.**
  Updating the app replaces the driver sources but never rebuilds the
  kernel module already installed, so a machine could keep an old `hp-wmi`
  indefinitely. An install now records which driver revision it built -
  the sha256 of the pristine `hp-wmi.c.orig`, with the upstream commit and
  date from `driver/README.md` - in
  `/usr/src/hp-wmi-omen-1.0/pyren-driver.json`, and `installer.inspect`
  reports it beside the bundled one as `driverVersion`. When they differ,
  the app shows a "driver update available" notice on opening, leading to
  the driver wizard; closing it lasts for that session, and its "don't
  show again" box (also a switch under Settings → Advanced) silences it
  for good. Installs made before this are identified by hashing their
  staged `.orig`, and one that cannot be identified - or whose build
  failed after its sources were staged - is reported as unknown rather
  than as up to date.
- **Force the keyboard light on at start.** A new switch at the bottom of
  the lighting page (`rgb.setForceBacklightOn`) makes the daemon switch
  the keyboard backlight on when it starts, in case the Fn key left it
  off - colours and effects written to a keyboard switched off that way
  never show. It uses the firmware's own backlight flag (`0x20009`, command
  types 4 and 5), confirmed on the OMEN 16-am0xxx. Only at start; off by
  default.
- **Effects pause while the Fn key has the keyboard light off.** Every
  frame of an effect is an EC transaction, and with the backlight switched
  off from the keypad none of them showed. While an effect runs, the
  daemon now reads the backlight switch every two seconds and pauses the
  effect while it is off (`throttled: "backlight"`), resuming when it
  comes back on. Nothing is read while no effect runs.
- **`tools/release.sh --dev`** for test builds: skips the version prompt
  (keeps whatever the manifests currently say), implies `--allow-dirty`,
  and names the archive `pyren-<version>-dev+<commit>-...` so it can't be
  mistaken for a tagged release.
- **Opt-in debug logging.** A new "Registros de depuración" switch in
  Settings (`debug.getStatus`/`debug.setEnabled`) keeps a rolling,
  human-readable history under `~/.cache/pyren/depuration` (or
  `/var/cache/pyren/depuration` for the installed daemon): driver/kernel
  identity whenever it changes, combined power+fan mode history, every
  RGB command sent, full calibration and diagnostic runs, the fan
  cleaner, driver installs, the daemon's own startup/shutdown, the full
  IPC transcript, and what the OSD widget and the app itself did. Off by
  default; five 5 MB files per category, one rotation generation each.
- **A preferred mode for each power source in the automatic switcher.**
  Under each auto switch on the home screen, choose what the supervisor
  treats as home: **Eco or Balanced on battery**, **Balanced or Performance
  when plugged in** (`preferredOnBattery` / `preferredOnMains` in
  `power.setAutoConfig`; `pyren-ctl power auto on --on-battery balanced
  --on-mains performance`). Unplugging or plugging in lands on it, and the
  supervisor only leaves it for a reason — sustained load to go up, an idle
  machine, heat or a low battery to go down — returning once load crosses
  the middle of the dead band back towards it, so a moderate load keeps
  whichever mode was asked for. Defaults: Eco on battery, Performance on
  mains. Performance is still never picked on battery, Eco never on mains,
  Unlimited never at all.
- **Automatic power-profile control in Settings.** A new panel gathers the
  whole supervisor setup: a master **Automatic control** switch (off: only
  the user changes the mode, not even plugging in or unplugging moves it),
  the two per-source switches and the thermal rule from the home screen,
  and the preferred mode on battery and on mains. The master switch keeps
  which per-source systems were on, and switching it back on starts the
  supervisor from scratch rather than acting on a cable moved while it was
  off. The Automatic/Manual selector on the Performance page now drives
  this switch; it used to write the OS-profile setting, which keeps its own
  checkbox below it.
  - **An Advanced fold** exposes the supervisor's tuning for the first time:
    the pause after a manual change (minutes), the low-battery threshold,
    the idle and busy load thresholds (percent per core) and the hot and
    cooled temperatures (in the chosen temperature unit), with a button
    that restores the defaults. `power.setAutoConfig` now refuses crossed
    load or temperature thresholds and a battery threshold outside 0–100
    (`power.err.loadBand`, `power.err.tempBand`, `power.err.batteryPercent`)
    instead of storing a config the supervisor would flap or latch on.
  - The two per-source switches are renamed to say what they do:
    **Automatic on battery (Eco ↔ Balanced)** and **Automatic when plugged
    in (Balanced ↔ Performance)**.
  - "Last automatic change" on the Performance page is hidden while
    automatic control is off.
- **A mode picked by hand is what the supervisor works around afterwards.**
  Once the manual pause (`manualOverrideSecs`) runs out, the supervisor no
  longer drifts back to its own choice: until the power source changes, it
  only steps *down* from the hand-picked mode (idle, hot, low battery) and
  back up to it when that passes. Performance chosen by hand on battery
  now survives a busy machine instead of being dropped to Balanced exactly
  when it was needed. The home screen says when this is in force, and
  `power.getState` reports it as `autoManualBaseline`. New reasons
  `power.autoReason.backToPreferred` and `power.autoReason.outOfRange`.

- **Fan control modes in the quick-access widget.** With the new
  **Settings → widget → "Fan control modes in the widget"** switched on (off
  by default), `pyren-osd` draws a second row of cards below the power
  modes — automatic, maximum, manual and curve — and switches them exactly
  the way it switches power modes: a click picks one outright, and the
  highlight follows a change made from the app or `pyren-ctl`. Picking
  **manual** reveals a 0–100 % slider for the fixed speed; the curve itself
  is still drawn in the app. On a machine that can switch fan modes but not
  command a speed only automatic and maximum appear. New `fan.mode` event
  (`{ mode, manualPwm, source }`), published on every `fan.setMode` the way
  `power.mode` is. The widget reads the setting from `~/.config/pyren/app.json`
  on each open, so the toggle needs no restart.
  - **The power-mode row can be turned off** ("Performance profiles in the
    widget"), but only while the fan row is on — for someone whose key is
    just a fan switch. The widget is never left with no rows.
  - **The widget now follows the pointer for when to close.** While the
    pointer is on it, it stays; it goes a second after the pointer leaves;
    and a click closes it two seconds later unless the pointer moves first
    (still choosing). Opened by the key with the pointer elsewhere, it
    keeps the old ~2.5 s glance.

- **Permissions can be revoked from the Permissions panel.** Each row on
  `/drivers` that the panel can grant now also offers **Revoke** once it
  is granted: stop and disable the service, leave the `pyren` group, unload
  `acpi_call` (and drop Pyren's boot-time drop-in), or delete Pyren's
  Coolbits snippet. File-based grants are only revoked when the file is
  Pyren's own. New `admin_grant` actions `leaveGroup`, `unloadAcpiCall`,
  `disableCoolbits`; new `admin_status` fields `leaveNeedsRelogin`,
  `acpiCallAtBoot`, `coolbitsOurs`.

- **Notifications in the app.** A bell in the header, with an unread badge,
  opens a box in the middle of the window listing what the daemon has
  reported — for now just the fan stall watch raising the fan floor
  (`fan.floorRaised`). History survives an app restart: it is reconciled
  from the daemon's persisted `floorNotices` on every telemetry poll and
  from the live event bus when the window is open, deduped by a
  content-derived id. A raise that reached the driver's own floor offers a
  shortcut to the drivers page to recalibrate. Read/unread is a
  `localStorage` convenience; "Clear" also empties the daemon's log via
  `fan.clearFloorNotices`. A live raise fires an OS notification through
  `tauri-plugin-notification` (permission requested on first use, a no-op
  outside Tauri).

- **Fans can run below the driver's 1800 rpm floor.** The upstream driver
  clamps every manual speed to its fan table's slowest entry, which is the
  bottom of the firmware's *automatic* curve rather than the slowest the
  fans can turn: with the clamp lifted, board 8D2F's fans held every speed
  down to 600 rpm exactly, and only stalled below ~500. The driver patch
  now reports the table's floor (`min_rpm_table`) and takes a replacement
  at runtime (`min_rpm_override`), still clamped, so no writer can ask for
  a stalling speed. `fan.calibrate` sweeps down from the driver's floor —
  200 rpm steps, then 100 below 1000 — commanding each step through that
  override so both fans get it exactly, and finds the slowest one held
  with no stall and no kick back up (a stalled motor being restarted).
  Pyren's floor is one 100 rpm step above that, because the edge moves
  between runs: 8D2F held 600 on one sweep and kicked from it on another,
  so its floor is 700.
  A new setting chooses between the two, **on the driver's by default**:
  Settings → Fans, `fan.setKeepDriverFloor`, `pyren-ctl fan floor
  driver|pyren`. `fan.getStatus` reports `driverMinRpm`, `pyrenMinRpm`,
  `keepDriverFloor` and `floorOverrideSupported`; `fanMinRpm` is now the
  floor in force. Needs a driver reinstall and a calibration.

- **The daemon watches for the fans stalling at Pyren's floor and raises
  it when they do.** That floor sits on an edge that moves — dust, a
  warmer bearing, a colder start — and when it moves up the fans stall at
  the commanded speed and the controller kicks them back into motion. On
  every control tick while Pyren's floor is in force, a commanded low
  speed that reads near zero or jumps back up is a fault; three in half an
  hour raises the stored held-speed one 100 rpm step (and Pyren's floor
  with it), tells the driver, and appends to `fan.getStatus`'s new
  `floorNotices` — which the app surfaces as a notification (see above).
  It only ever raises, never past the driver's own floor, and waits
  five minutes between raises. `fan.floorRaised` carries it on the event
  bus too; `fan.clearFloorNotices` (`pyren-ctl fan notices clear`) empties
  the log; a full `fan.calibrate` re-measures the floor and starts it
  clean.

- **0 % can stop the fans.** On boards whose fans have a floor — 8D2F's
  driver cannot command less than 1800 rpm, so everything from 0 to a
  third of the scale sounded the same — a curve or manual speed below that
  floor now hands the fans to the firmware, which stops them while the
  machine is cool and spins them up on its own if it is not. They come
  back under the curve once it climbs a deadband clear of the floor.
  `fan.calibrate` measures the floor after the ceiling (`fanMinRpm`); until
  it has, nothing changes. `fan.getStatus` reports `fanMinRpm`,
  `stopBelowPwm` and `fansReleased`, and the performance page shades the
  band on the curve and says when the firmware has the fans.

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

- **The fan cleaner no longer reverses fans that did not stop.** The
  braking step used to run out of time and engage reverse anyway; it now
  waits up to 7 seconds (was 4), and if either fan still reads 300 RPM or
  more it releases the brake and refuses the cycle
  (`fan.cleaner.err.brakeFailed`).
- **Stopping the fan cleaner no longer commands reverse spin by itself.**
  When the firmware did not report a reverse speed, the ramp down started
  from an assumed 3700 RPM - on the paths that stop fans nobody confirmed
  were reversed (a failed start, daemon exit), that briefly reversed them.
  The ramp now starts from the firmware's speed or a tachometer reporting
  reverse, and is skipped when there is neither.
- **The fans are handed back only once they turn forwards again.** After
  releasing the override the cleaner watches the tachometers for up to 3
  seconds rather than waiting a fixed 2, so the control loop does not write
  a forward speed onto blades still coasting backwards.
- **The fan cleaner's ramp down no longer writes a third fan's speed on a
  machine with two.** The byte is commanded only where the firmware's
  capability mask claims a third fan, as the start sequence already did.
- **Manual and curve fan modes did nothing on boards in the driver's
  feature table** (8D2F among them, once the installer adds it). On those
  boards `pwm1_enable = 1` replaces both setpoints with the speed the fans
  are turning at, and at 0 rpm that is the driver's "automatic". Pyren
  wrote the speed first and the mode second, on every tick, so each step
  was undone the moment it was made. The mode now goes first and only when
  the driver is not already in manual — the order the Python original
  uses — and `pwm2`, the GPU fan, is written too. A `speedControl:
  ignored` stored on such a board before this fix was very likely this bug
  rather than the embedded controller; re-run `fan probe-speed`.
- **Every manual speed reached the fans 100 rpm slow.** The driver's
  pwm ↔ rpm conversions truncate, and a write goes through three of them:
  on board 8D2F, `pwm1 = 128` of a 5300 rpm fan sent 2500 rather than
  2700, and the fan table's slowest entry, 1800, went out as 1700. The
  installer now patches both conversions to round, which makes the round
  trip exact; a driver reinstall picks it up.
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
- **Docs: the `fan.diagnose` section of the IPC protocol reference was
  stale.** It still described the old readback-only write check and listed
  neither `pwm-write` nor `pwm-effect`. It now names every check by `id`,
  notes that `allowWrites` briefly spins the fans (a `fan.probeSpeedControl`
  run `diagnose` fires itself), and spells out how the verdict follows the
  check results.
- **The light strip is no longer auto-picked on a machine that has none.**
  Its read answers `PASS` on four-zone keyboards that have no strip (8D2F
  is one), so when no other lighting protocol answered, `lightbar` was
  chosen and lit nothing. The firmware is now asked its keyboard type
  first (`0x20008` / `0x2b`), and any type but per-key leaves the strip
  unasked - the rule upstream's driver adopted in `2d3f2a4`. Pinning
  `lightbar` by hand still asks it, and a keyboard-type read that fails
  changes nothing.

### Changed

- **The vendored `hp-wmi` is updated to upstream `2d3f2a4`** (was the
  2026-09-04 copy). It adds `fan*_max`, a `keyboard_type` attribute, and
  multicolour LEDs for the keyboard zones (`hp::kbd_*`) and the light
  strip (`hp::lightbar-N`); a driver reinstall picks it up.

- **Unplugging now lands on Eco by default rather than Balanced**, the new
  battery preference; set it to Balanced to keep the old behaviour.

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
