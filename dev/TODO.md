# TODO

Priority order within each section. Each item says what blocks it and how
to tell when it's done. Items marked **[HP]** need the HP laptop — which
*is* the development machine as of 2026-09-02, so what they now need is
root on it, not different hardware.

---

## 1. Do next

### 1.1 Try the patched driver on 8D2F **[HP]**
This is now the *only* thing standing between this laptop and a real fan
percentage, and the reasoning changed: reading the driver source showed
that none of the three `pwm1_enable` modes is gated on the board-params
table, and that `hp_wmi_hwmon_is_visible` returns `0644` for `hwmon_pwm`
unconditionally. See `FINDINGS.md` §"What the driver actually gates on the
board table". So installing it should expose `pwm1` here, where the running
7.2.2 driver does not — and whether the firmware honours the write is then
an experiment, not an inference.

Everything the daemon needs is already built and refuses to run on this
hardware only because `pwm1` is absent (`capabilities.setSpeed`), so this
is a one-variable test:

1. `installer.plan` / `installer.apply` with `experimentalBoard: "8D2F"`
   and a `boardTable` choice — the variants differ in which EC offset
   holds the thermal profile, so the wrong one loads and misreads. The
   driver wizard at the bottom of `/drivers` now drives both, including
   those two fields, and refuses to apply until the dry run of that exact
   plan has come back — which is the safer way to do this experiment than
   hand-written IPC.
2. `sh tools/pyren-check.sh` and look for `pwm1`.
3. `fan.setMode` with `{"mode":"manual","pwm":128}` and listen.

*Done when*: `capabilities.setSpeed` is true, or it is established that
the firmware refuses the query.

**This is also the first run of the installer's execution path**, which has
never been executed. Take a backup of the stock module first — the plan
does this itself (`backup-driver`), but knowing where it went matters:
`hp-wmi.ko.bak` beside the original.

### 1.2 Run `fan.calibrate` on the laptop **[HP]**
The routine is written, unit-tested and wired into `pyren-ctl`; it has
**never been run against hardware**, and unlike 1.1 nothing is blocking
that but a root daemon. It needs only `switchMode`, which this board has,
so it is the cheapest hardware experiment left:

```sh
cd daemon && sudo -E cargo run -p pyren-daemon      # terminal 1
cargo run -q -p pyren-ctl -- fan calibrate          # terminal 2, loud
```

*Done when*: `fanMaxRpm` is a number in `fan.getStatus`, and the sample
trace in the reply is in `FINDINGS.md` next to the `max`/`auto` one. Worth
doing **before** 1.1 rather than after: it is the number the installer's
`cpuMaxRpm`/`gpuMaxRpm` patch wants, and measuring it with the stock driver
means the patched one can be given a real value on its first build.

### 1.3 Ask the firmware about the lightbar **[HP]**
The `rgb` module is written (`daemon/crates/rgb`) and has **never been run
against a light strip**, for one installable reason: `/proc/acpi/call` does
not exist here because `acpi_call` is not installed. Everything that can be
tested without it is — the 144-byte buffer the port builds, the replies it
accepts, the probe on a machine with neither path — so this is a
one-variable test like 1.1:

```sh
sudo pacman -S acpi_call                          # prebuilt for this kernel
cd daemon && sudo -E cargo run -p pyren-daemon    # terminal 1
cargo run -q -p pyren-ctl -- rgb probe            # terminal 2
cargo run -q -p pyren-ctl -- rgb set '#ff9900'
cargo run -q -p pyren-ctl -- rgb read             # did it understand the payload?
```

*Done when*: `rgb probe` says whether the firmware answered, and either
answer is in `FINDINGS.md`. **"The firmware refused" is a result**, and the
one that stops the next person re-deriving this — the payload constants are
upstream's reverse engineering and nobody has confirmed them on any
machine.

The per-key path stays unported until a `0d62:54bf` turns up somewhere and
the review's finding 1 can be settled on it; the `keys.json` half of that
finding is already confirmed (`FINDINGS.md`).

---

## 2. Blocked on a decision or on hardware

### 2.1 Driver sources: vendor, submodule, or fetch?
Analysis in `FINDINGS.md`. Currently the installer looks for a checkout and
reports a blocker when it finds none, which is honest but means the driver
path only works for someone who already has the other repo. **Needs a call
from the project owner**, and is low priority while fan control is upstream
anyway.

### 2.2 GPU switching, network booster, key mapping
The UI for all three is complete and drives local state only. Each needs a
real backend decision before any daemon work:

- **GPU switching**: wrap `supergfxctl`, or implement it directly? Either
  way it needs a session restart, which the UI already says.
- **Network booster**: per-process traffic accounting plus `tc`/`nftables`
  rules. This is the largest of the three by far, and arguably the least
  valuable — consider dropping the page rather than building it.
- **Key mapping**: `keyd`, `udev` hwdb, or an evdev-level remapper. Affects
  whether the daemon needs to hold an input device open.

---

## 3. Worth doing, not urgent

- **Logging.** ~20 `println!`/`eprintln!` calls. A level-filtered logger
  would let the daemon be quiet under systemd and verbose when diagnosing.
- **Temperature in the power supervisor.** It refines on load and battery
  charge only; a hot chassis is a good reason to back off.
- **Import the Windows OMEN profile.** The one honest source of per-machine
  envelope numbers that exists: `PowerControlConfig.json` on the Windows
  partition (gzip'd UTF-16 JSON) holds what HP itself considers this
  chassis's Eco and Performance. Worth more than any default this project
  could invent.
- **What the firmware profile does that we cannot.** Changing
  `platform_profile` moves the EC's temperature-to-RPM curve (Eco makes the
  fans start *later*, not just spin slower) and internal power states such
  as PCIe link power. On a machine with no firmware profile — board 8D2F —
  none of that is reachable, and the envelope is all there is. Exposing
  PCIe ASPM policy (`/sys/module/pcie_aspm/parameters/policy`) as an
  explicit, off-by-default setting would recover a little of it; it is not
  something to switch on behalf of anyone, and it is not a fan curve.
- **Reaching Eco and Balanced tuning from the UI.** The power-limit sliders
  live in the power sub-tab, which the reference app only shows under
  Performance and Unlimited — so the Eco and Balanced profiles can only be
  tuned over IPC (`power.setTuning` takes a `mode`). The defaults are
  reasonable, but a user who wants a 25 W Eco cannot get there by clicking.
- **A second reference sensor.** The curve follows the CPU only. The
  original also supports GPU, with a fallback to CPU when the GPU reads 0
  because it is asleep; `FanConfig` has no `referenceSensor` field yet.
- **Fan cleaner** (reverse spin, `acpi_call`) — the protocol is documented
  in the source project, and it's the one genuinely novel feature. The
  `acpi_call` plumbing it needs already exists: `pyren_core::acpi`, with
  the cross-module lock. Reach for that rather than opening the file.
- **Per-process GPU usage** in the vitals table (the column exists and
  shows `--`).
- **Packaging**: nothing exists. PKGBUILD first, given the audience.
- **CONTRIBUTING.md**, including how to add a translation (the mechanism is
  already documented in `docs/03-frontend.md` and the Help page).
- **More locales.** Only `en` and `es`; adding one is dropping a JSON file
  in `app/src/lib/i18n/locales/`.
- **Accessibility pass** on the frontend: keyboard navigation through the
  mode cards and the fan-curve editor, focus visibility, reduced motion.
- **End-to-end IPC test**: spawn the daemon on a temp socket, exercise every
  module method. Would have caught the Tauri command wiring being untested
  for two sessions. Now also the natural place to prove the socket's
  permissions from the outside — the unit tests assert the mode bits, but
  only a second user can prove the *effect*, and CI has no second user.
- **`shellcheck` in CI.** The workflow runs `sh -n tools/pyren-check.sh`,
  which catches syntax and nothing else. `shellcheck` wasn't added because
  it isn't installed here and a lint nobody has run locally would land red.

### GPU overclocking — what is left of it

The module landed (see §"Done"). What remains needs hardware or root
rather than design:

- **No offset has ever been written, on any machine.** The development
  laptop reads both NVIDIA offset attributes and refuses to be written to
  them - its X screen has no `Coolbits`, and it is a Wayland session - so
  the climb, the read-back and the revert have been exercised against
  refusals and never against a card that says yes. Anyone with `Coolbits`
  set: `pyren-ctl oc probe --write` first, then a small `oc set --core 15`.
- ~~The clock lock has not been run as root.~~ **Done, and it works**:
  as root, 900-1200 MHz took the idle card from 180 MHz / P8 / 7.5 W to
  892 MHz / P5 / 9.9 W, and letting the confirmation lapse put it back on
  its own. The revert timer has now run against a real GPU, not only
  against refusals.
- **A root daemon cannot reach the offsets on a Wayland desktop.** Not
  `Coolbits` this time: the X server admits the uid that owns it, and the
  compositor starts `Xwayland` with no `-auth` file, so there is no cookie
  to point `PYREN_XAUTHORITY` at. Either the user runs
  `xhost +si:localuser:root` in their session, or whatever sets the
  offsets has to run inside it. Worth deciding *before* an offset is ever
  written: it may be that the honest answer is "offsets need Xorg with
  Coolbits", and the module already says so in words.
- **AMD Overdrive is detected and not driven.** `pp_od_clk_voltage` is a
  two-line write away and stays unwritten until there is an AMD machine to
  test on: a wrong value there does not fail with an error message. The
  probe already says so in words.
- **Raising CPU PL1/PL2 above stock** belongs behind the same consent, and
  is deliberately not in the module yet: the `power` module owns those
  registers and re-applies them, clamped to stock, on every mode change.
  Doing it means giving one of the two modules ownership of the envelope,
  which is a decision, not an addition.

---

## 4. Deliberately not done

Recorded so nobody "fixes" them by accident:

- **No read-only access for non-members of `pyren`.** The socket is
  `0660`; a user outside the group gets nothing, not "vitals but no
  writes". Splitting reads from writes would mean opening a root daemon's
  socket to every local process — sandboxed and compromised ones included —
  to save an admin one `usermod -aG`. The protocol also cannot tell two
  group members apart, so no future method may depend on *which* one
  called.
- **No mode ships power-limit numbers.** Every laptop has its own internal
  profiles and their curves are not each other's, so a percentage that is a
  sensible Eco on one chassis is a throttled mess on the next. Out of the
  box a mode drives only the mechanisms the machine provides; the envelope
  is left where the firmware set it until someone puts in a number they
  measured. Inventing one and applying it everywhere would be worse than
  doing nothing, because it would look deliberate.
- **A power profile never exceeds the firmware's own limits.** Eco caps,
  Performance and Unlimited restore; neither raises. "Unlimited" means this
  daemon imposes no limit of its own, not that the machine is unlocked —
  going past stock is overclocking, and belongs behind its own consent.
- **The installer creates the `pyren` group but never deletes it.**
  Removing a group that users are still members of is not the installer's
  call, and a leftover empty group costs nothing.
- **The daemon does not touch the fans until asked.** At startup it reports
  the mode the hardware is *in* rather than the one in its config, and
  writes nothing. Adopting an observed `manual` and then "re-asserting" it
  would put our idea of the speed over whatever the user had set, seconds
  after boot, for no reason. `restoreModeOnStart` is the opt-in, and like
  the power module's equivalent it defaults to off.
- **`auto` is written once, not re-asserted.** In auto the firmware owns
  the fans and the driver cancels its own keep-alive; re-issuing it every
  minute would be a WMI call that changes nothing. Max, manual and curve
  are re-asserted every 60 s.
- **No `core.json`.** Cross-cutting daemon config has no contents yet.
- **`system` always reports `supported: true`** as a *module*. Any Linux
  machine can report its own vitals; hardware-*control* support is a
  different question, answered by `system.getInfo`'s `controls`.
- **No board allowlist anywhere.** What a machine can do is probed, never
  looked up. A list of DMI ids cannot know whether the driver came up with
  reduced functionality on this boot, has to be extended by hand, and — on
  the one machine available to test — was simply wrong. If something ever
  needs a board id again, it must be advice attached to a *probe result*,
  not a substitute for probing.
- **The installer refuses to run when fan control already works**, unless
  forced. Replacing a working stock driver is a downgrade.
- **`restoreModeOnStart` defaults to off.** Changing a machine's power
  behaviour at boot should be something the user asked for.
- **`apply` is a dry run unless `confirm: true`.** A mis-sent IPC message
  must not be able to replace a kernel module.
- **localStorage is a cache, not a record.** It exists so the first frame
  has the user's language; the file on disk always wins.

---

## Original notes

The root `TODO` file's items have been folded in here. For the record, all
five of its original entries are now done: the DMI reader and compatibility
check (`system.getInfo` + the Help page), the driver-missing notice with a
remembered "don't show again", the help/legal section, the version and
update check, and the documentation of how to contribute translations. The
sixth, from the Obsidian notes — a background system that switches between
Eco and Balanced on its own — is the `power` supervisor.

## Done since these notes were written

Newest first. Kept because the *reasons* are the useful part - several of
these replaced an earlier version of themselves, and knowing why saves
someone re-proposing it.

- **GPU overclocking** (was §3's "last on purpose"): the `overclock`
  module, plus the page that drives it. The four conditions this file set
  for it are what most of the code is: an offset defaults to zero, an
  offset is never restored at boot without an explicit opt-in, an apply is
  a climb in 15 MHz steps with a revert-on-failure timer, and the warning
  is the daemon's own text rather than something the app can reword.
  Decisions worth keeping: **the timer is a thread, not a check on the next
  call**, because the case it exists for is exactly the one where no next
  call arrives - the desktop is gone and the only thing still running is a
  root daemon with a deadline; **the armed flag is persisted**, so a daemon
  that starts and finds it set knows the machine went away while
  overclocked and restores nothing that boot (`unconfirmedAtStart` says
  so); **a knob that is known not to be writable is withdrawn from the
  state**, since a slider that can only fail is worse than no slider; and
  `reset` is refused in no state of the module, consent included, because
  "put it back" must always work. `nvidia-smi --lock-gpu-clocks` is
  included even though it is *not* an overclock - it cannot exceed what the
  card ships with - because it is the only mechanism this laptop has and it
  is the knob somebody on that page usually wants. The write-probe
  (`oc probe --write`, a no-op assignment of the current offset) is what
  turned "the offsets are readable" into "the offsets are readable and not
  settable, because there is no Coolbits here".

- **The driver installer wizard**, the last unbuilt piece of the frontend
  (`docs/03-frontend.md` §"Not built yet", roadmap item 6): `/drivers` now
  ends in a collapsed panel that runs `installer.inspect`, renders the plan
  step by step with the command each one would run, and only then offers to
  run it. Three decisions worth keeping: **apply is disabled until a dry
  run of the same options has come back**, with the options serialised into
  a key so that typing in any field discards the plan and the report — a
  plan shown next to options that no longer produced it is the failure mode
  a wizard exists to prevent; the panel is **closed by default and leads
  with `patchNeeded`**, because on a modern kernel the honest answer is "you
  do not need this" and an install button above that sentence is an
  invitation to downgrade a working driver; and the fan-ceiling field
  **offers the calibrated number rather than a default**, blank meaning
  "keep the driver's own fallback". The Tauri side passes the request
  through as opaque JSON (`installer_inspect`/`plan`/`apply`), so adding a
  field to the daemon's request does not mean editing three layers.
  **Still never executed against hardware** — that is 1.1 above, and the
  wizard is now the way to run it.

- **Structured IPC errors** (was 1.2): a refusal is now
  `{ kind, message }` instead of a sentence, with eleven kinds and one rule
  - branch on `kind`, show `message`. The distinction that pays for it is
  `notCapable` against `permissionDenied`: the first will never work on
  this board however it is asked and the second works fine as root, and a
  UI that conflates them either offers to elevate for hardware that will
  never comply or reports working hardware as broken. `ModuleError::Other`
  is gone - all 31 uses were reclassified, which was most of the work and
  the actual point, since a catch-all variant is how the prose happened.
  `pyren-ctl` turns the kind into an exit code (2/3/4/5/6) so a script can
  branch without reading English. Two decisions worth keeping: an unknown
  kind is treated as `failed` rather than refused, because a client that
  cannot parse a newer daemon's refusal is worse than one that shows it;
  and both clients still accept a bare-string `error`, because reading an
  unparseable error as *absent* would turn a refusal into a silent success
  - which is exactly what `and_then(Value::as_str)` would have done.
  **The app only carries the message through**: a Tauri command's error is
  a string and nothing branches yet. Wiring the kind into admin mode is the
  obvious next step and belongs to whoever owns that flow - `notCapable` is
  the case where "run as administrator" must *not* be offered.

- **Calibration** (was 1.3): `fan.calibrate` - max, watch the fans, keep
  the peak, put back the mode it found - plus `fan1MaxRpm`/`fan2MaxRpm` in
  `fan.json` and `pyren-ctl fan calibrate`. Three decisions worth keeping:
  it **stops as soon as the reading settles** (the fans reach ~3900 rpm in
  six seconds here, so a fixed thirty is twenty-four seconds of noise that
  measures nothing); a run that did not move the fans **stores nothing**,
  because a machine's idle speed recorded as its ceiling is worse than no
  calibration at all - the hysteresis would believe every target above idle
  was already reached; and the restore is a `Drop` guard rather than a line
  at the end, falling back to `auto` when the observed mode cannot be put
  back, because leaving a machine at full speed is the worse failure. It
  needs only `switchMode`, so it runs on 8D2F. **Not yet run on hardware** -
  that is now 1.3 above. No app UI either: the fan pages were being edited
  in another session when this landed.

- **`pyren-ctl`**, a shell client over the same socket: `status`,
  `power set/tune/auto/os-profile`, `fan set/curve/diagnose`, `--json` on
  anything. Exists mainly so a measured number can be recorded without a
  slider. The wire format now has one client implementation
  (`pyren_core::client`) rather than one per caller; the Tauri app still
  carries its own, being a separate workspace.
- **The laptop's profile and the OS's are applied separately.** The app has
  had an "apply to the OS power profile" switch since before the daemon
  honoured it; it does now (`power.setApplyToOsProfile`). The OS half is
  delegated to power-profiles-daemon rather than reimplemented, and the
  per-CPU EPP hint is only a fallback for machines without it - writing it
  alongside PPD was two things fighting over the same files.
- **The auto-switch supervisor matches the reference app**: two systems,
  one per power source. Unplugging drops to Balanced at once and plugging
  in steps up to Performance at once; from there each refines within its
  own range (Eco↔Balanced on battery, Balanced↔Performance on mains) as
  load and battery charge hold. Unlimited is never chosen automatically. A
  manual choice suspends refinement but not transitions.
- **The power modes are profiles now**, not a single switch: alongside the
  OS preference there is a package power envelope (PL1/PL2 and turbo),
  which is the half the fans feel on a machine with no firmware profile.
  `power.setTuning` edits it, in watts, stored as a percentage of the
  machine's own stock limits. It **ships untouched** — the first version of
  this shipped invented percentages, which was wrong for the reason in §4.
  The supervisor applies the whole profile too: a mode has to mean the same
  thing whether the user picked it or the auto-switcher did.
- **The compatibility verdict is now measured.** `system.getInfo` reports
  `controls` — what the fan and power modules found they could actually do
  — and `compatibility` is only their summary. `crates/system/src/boards.rs`
  and its 80-odd hand-copied board ids are gone.
- **Max and auto verified on the hardware** (was 1.2): against a root
  daemon, `fan.setMode max` took the fans from ~2000 to ~3900 rpm and
  `auto` handed them back. The numbers are in `FINDINGS.md`. This is the
  project's first real hardware write.
- **The socket restriction verified end to end** (was 1.1): with the daemon
  running as root, a process not in the `pyren` group gets
  `PermissionDenied` on connect, and the same process reaches it through
  `newgrp pyren`. The socket is `srw-rw---- root pyren`.
- **The fan write path** (was 1.4): `fan.setMode`, `fan.setCurve` and
  `fan.setRestoreOnStart`, a control loop that follows a curve on its own
  thread, hysteresis, temperature smoothing, and `fan.json` persistence.
  Split so that the arithmetic (`curve.rs`) and the hardware semantics
  (`control.rs`) are testable without an HP laptop, which is most of it.
  The `pwm1`-dependent modes correctly refuse to run on this board; see
  1.1 and 1.2 above for what is left, both of which need hardware rather
  than code.
- **The Tauri fan commands existed only on the frontend's side.**
  `daemon.setFanMode` called `fan_set_mode`, which was never registered in
  `invoke_handler` — every call failed at the bridge. Now wired, along with
  `fan_set_curve` and `fan_set_restore_on_start`. Exactly the gap the
  end-to-end IPC test in §3 is meant to catch.
- **The daemon socket is restricted** (was 1.1): bound `0660` to the
  `pyren` group, with the runtime directory locked down when the daemon
  creates it, an actionable startup message when the group is missing, and
  an `EACCES` from the app turned into "add this user to the group". The
  installer creates the group as its first step.
- **LICENSE** (was 1.2): GPL-3.0-or-later for the whole repository.
  `app/package.json` and the Help page said MIT; they now agree with the
  `Cargo.toml`s. The choice is not free — the `fan` and `installer` modules
  are ports of a GPL-3.0 project.
- **Continuous integration** (was 1.5): `.github/workflows/ci.yml`, four
  jobs — daemon (`cargo test` + `clippy -D warnings`), app (`svelte-check`
  + `vite build`), Tauri shell (`cargo check`), and `sh -n` on the shell
  script. The parity test runs under `dash` there rather than the
  developer's shell, which is the point of a POSIX script.
