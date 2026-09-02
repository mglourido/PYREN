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
   holds the thermal profile, so the wrong one loads and misreads.
2. `sh tools/pyren-check.sh` and look for `pwm1`.
3. `fan.setMode` with `{"mode":"manual","pwm":128}` and listen.

*Done when*: `capabilities.setSpeed` is true, or it is established that
the firmware refuses the query.

**This is also the first run of the installer's execution path**, which has
never been executed. Take a backup of the stock module first — the plan
does this itself (`backup-driver`), but knowing where it went matters:
`hp-wmi.ko.bak` beside the original.

### 1.2 Structured IPC errors
The socket now returns a *permission* failure the app has to recognise, and
`fan.setMode` returns three different kinds of refusal — unsupported
hardware, bad params, needs root — all as prose. The app gets away with
matching on `io::ErrorKind` for the connect, but nothing can branch on the
rest. `docs/01-ipc-protocol.md` already warns against string-matching them.
Move to `{ kind, message }` before any caller needs to.

### 1.3 Calibration
`fanMaxRpm` is the one input the hysteresis wants and never has, so it
currently falls back to comparing PWM values instead of RPM. The routine is
specified in `docs/04-fan-control-logic.md` §Calibration (max for 30 s,
read the fans, restore) and is worth doing right after 1.1, because on this
board it is also the honest way to find out what full speed *is*.

---

## 2. Blocked on a decision or on hardware

### 2.1 RGB module
**Which port to write is now decided**: `lsusb` on the laptop finds no
`0d62` device, so the per-key USB HID path has nothing to talk to and the
4-zone ACPI lightbar is the only candidate (`FINDINGS.md` §"The test laptop
has no per-key RGB keyboard"). Review and porting order are in
`docs/04-rgb-porting-review.md`.

Still blocked, but on something installable rather than on an unknown:
`/proc/acpi/call` does not exist here because `acpi_call` isn't installed
(`acpi_call-dkms` on Arch). Install it, then confirm the lightbar answers
before writing the module — "no per-key device" is proven; "the 4-zone
interface works on this machine" is not.

When it is written: `/proc/acpi/call` needs a **cross-module** lock (the fan
cleaner uses it too), so that belongs in `core` or a new shared crate, not
inside the `rgb` module.

### 2.2 Driver sources: vendor, submodule, or fetch?
Analysis in `FINDINGS.md`. Currently the installer looks for a checkout and
reports a blocker when it finds none, which is honest but means the driver
path only works for someone who already has the other repo. **Needs a call
from the project owner**, and is low priority while fan control is upstream
anyway.

### 2.3 GPU switching, network booster, key mapping
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
  in the source project, and it's the one genuinely novel feature.
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

### GPU overclocking — last on purpose

The last piece of the profiles, and deliberately last. Everything the power
module does today stays **inside the envelope the firmware shipped**;
overclocking is the first feature that would leave it, so it needs more
than a slider:

- The UI already has core/memory offset sliders driving local state only
  (`system/advanced`), so the temptation to wire them up quickly is real.
  Don't: an offset that is stable in a benchmark and not in a game is the
  normal case, and the failure mode is a hang or corrupted VRAM, not an
  error message.
- NVIDIA offsets go through `nvidia-settings`/NVML and need coolbits or the
  newer `nvidia-smi -lgc`/`--lock-memory-clocks`; the mechanism differs by
  driver version, so probe rather than assume.
- Whatever lands must: default to zero offset, never restore an offset at
  boot without an explicit opt-in (unlike the power modes, a bad offset can
  stop the machine booting to a desktop), apply in small steps with a
  revert-on-failure timer, and say plainly in the UI that this is the one
  feature that can damage a session's work.

Raising CPU PL1/PL2 above stock belongs in the same bucket and under the
same consent, not in the power profiles.

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
