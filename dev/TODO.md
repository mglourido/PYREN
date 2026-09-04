# TODO

Priority order within each section. Each item says what blocks it and how
to tell when it's done. Items marked **[HP]** need the HP laptop — which
*is* the development machine as of 2026-09-02, so what they now need is
root on it, not different hardware.

---

## 1. Do next

### 1.1 Run `fan.calibrate` on the laptop **[HP]**
The routine was written, unit-tested and wired into `pyren-ctl` long before
anything ran it against hardware. It needs only `switchMode`, which this
board had even before the driver was patched, which is what made it the
cheapest hardware experiment left:

```sh
pyren-ctl fan calibrate          # loud, ~20s, puts the mode back itself
```

**Done, 2026-09-04.** `fanMaxRpm` is **5200**, both fans, and `fan get`
reports "5200 rpm, measured". The trace is in `FINDINGS.md` §"Full speed on
this machine is 5200 rpm". It restored `auto` cleanly, which is the half of
the routine that had never met a real `switchMode`.

What is left of this item is the *second* sentence it used to end on: 5200
is the number the installer's `cpuMaxRpm`/`gpuMaxRpm` patch wants, and the
driver was installed without it, so it is still running on whatever ceiling
the firmware reports. Reinstalling with `--cpu-max-rpm 5200 --gpu-max-rpm
5200` would pin a measured one. Filed under §3, not here: it is a
reinstall, and nothing is visibly wrong without it.

### 1.2 Ask the firmware about the lightbar **[HP]**
The `rgb` module is written (`daemon/crates/rgb`), the app's lighting page
now drives it, and the whole stack has **never been run against a light
strip** — for one reason, and it is now smaller than it was. `acpi_call` is
**installed** on this machine (DKMS, built against `linux-cachyos`); it is
simply not loaded, so `/proc/acpi/call` does not exist yet. Everything that
can be tested without it is — the 144-byte buffer the port builds, the
replies it accepts, the probe on a machine with neither path — so this is a
one-variable test:

```sh
sudo modprobe acpi_call                           # already installed here
cd daemon && sudo -E cargo run -p pyren-daemon    # terminal 1
cargo run -q -p pyren-ctl -- rgb probe            # terminal 2
cargo run -q -p pyren-ctl -- rgb set '#ff9900'
cargo run -q -p pyren-ctl -- rgb read             # did it understand the payload?
```

**Done, 2026-09-04.** The firmware answers, the lights change, and the
answer is in `FINDINGS.md` §"The lights work, and what was in the way was
our own read". The short version: the blocker was not HP but
`fs::read_to_string` on `/proc/acpi/call` returning nothing; this machine
speaks the `fourZone` dialect; and the `lightbar` dialect answers `PASS`
while doing nothing, which is why "the firmware accepted it" is not
evidence a dialect works.

### 1.2b Get the zone-4 read back **[HP]**
`acpi_call` truncates a 128-byte reply to its first 34 bytes, so zone 4
starts one byte past the end. The colour written to it is real; it just
cannot be read back, and a write pads the unseen tail of the state buffer
with zeros. The fix needs no more reverse engineering - it is the
`kernelZones` dialect, which needs no `acpi_call` at all and which this
build already speaks.

Two things this item assumed about `omen-rgb-keyboard` are wrong, both read
out of the clone on 2026-09-04 and both written up in `FINDINGS.md` §"What
`omen-rgb-keyboard` actually costs":

- It publishes under `/sys/devices/platform/**omen-rgb-keyboard**/rgb_zones`,
  not under `hp-wmi`. **Fixed**: `kernel_zones::dir()` now searches both
  names, so whichever module publishes them is found.
- It ships `blacklist hp_wmi` and its README says to unload `hp_wmi`. Every
  fan control we have - including the 5200 rpm from §1.1 - goes through
  `hp-wmi`'s hwmon, and this machine's `hp-wmi` is the *patched* one. So
  the blacklist is not an option, and the experiment is whether the two can
  be loaded at once.

**Done, 2026-09-04.** `tools/try-kernel-zones.sh` installed the module
through DKMS and loaded it **with `hp_wmi` still in place**, against the
module's own README. Both worked at once: `hp-wmi`'s `pwm1` kept answering,
and `rgb read` now reports four real colours — `#f9350f` in zone 4, where
the truncated `acpi_call` reply could only ever say black. `rgb get` shows
`dialect kernelZones (chosen automatically)`, with nothing pinned and no
daemon restart needed. Written up in `FINDINGS.md` §"`kernelZones` works,
and `hp_wmi` did not have to go".

Was the module worth it, as this item asked? Yes, and the reason is not
zone 4 by itself: `kernelZones` writes one file per zone, so it also
retires the hand-built 144-byte buffer and the zero-padded tail we were
guessing at on every write.

The per-key path stays unported, and is now blocked twice over: no
`0d62:54bf` device on this machine to settle the review's finding 1 on, and
**the upstream source is gone** — `src/driver.py` on the USB stick is zero
bytes, along with the rest of `src/`. `data/keys.json` and the README
survive, so the key map and the SDK's shape are recoverable; the HID report
layout is not. Porting it now means re-fetching `omen-rgb-linux` from
GitHub, not reading the stick.

---

### 1.3 The trigger is a shortcut, not Fn+P — settled 2026-09-04
Closed by a decision rather than by an experiment. The trigger is set in
the app's Settings and is currently `Ctrl+Shift+P` (keycode 25 on the AT
keyboard); `hotkey learn` binds it and the daemon acts on it. Whether the
EC ever lets Fn+P reach Linux is no longer on the critical path, and this
item should not be reopened to find out.

Everything downstream of the trigger works: `hotkey press` raises the OSD,
and `presses` counts real keys only — it stays at 0 for a synthetic press,
so a non-zero value is evidence a real key arrived.

### 1.4 Package `pyren-osd`
Starting it is done: the app spawns it at launch, and Settings → Services
has the "start at login" switch, which writes
`~/.config/systemd/user/pyren-osd.service` pointing at whichever binary was
found.

**Written, 2026-09-04**: `tools/install.sh` puts `pyren-osd` — and
`pyren-ctl`/`pyren-daemon`, where they are built — in `/usr/local/bin`,
which is the first place `find_osd()` looks outside the build tree, and
installs `osd/pyren-osd.service` to `/usr/lib/systemd/user`. That last one
is the difference between the app writing somebody a unit into `$HOME` and
`systemctl --user enable pyren-osd` simply working. `--dry-run` says what
it would do and `--uninstall` takes it back out, leaving the app's own
`~/.config/systemd/user` copy alone.

**Done, 2026-09-04.** Run once as root here: all three binaries are in
`/usr/local/bin` and answer `--version` from a shell with no build tree in
it, and `systemctl --user list-unit-files` shows `pyren-osd.service`. One
defect the run found and fixed: the closing `systemctl --user
daemon-reload` ran as root under `sudo`, which reloads root's user manager
rather than the caller's. It now drops back through `SUDO_USER`.

---

## 2. Blocked on a decision or on hardware

### 2.1 Network booster, key mapping
- **Key mapping**: still local state only. Needs a real backend decision -
  `keyd`, `udev` hwdb, or an evdev-level remapper - which affects whether
  the daemon needs to hold an input device open.

**Network booster decided and built (the honest half), 2026-09-04.**
Per-process traffic accounting plus per-PID `tc`/`nftables` rules — what
the original mock table needed — stays undone: it needs cgroups,
`nftables` socket matching or eBPF to attribute a packet to a process at
all, none of which this daemon has, and it was already flagged here as the
larger and less valuable half. That table (priority dropdown, block
button, "double force") is now gone from the page rather than left
pointing at nothing.

What *is* real: one system-wide knob. `off` deletes the default-route
interface's root qdisc; `auto` hands it `cake`, falling back to
`fq_codel` on a kernel with no `sch_cake` — both fair-queue by flow, so a
big transfer does not drown out a game or a call sharing the same link,
with no per-process bookkeeping needed. Built: `daemon/crates/network`
(`network.getStatus`/`network.setMode`, see `docs/01-ipc-protocol.md`
§"`network` module"), registered in the daemon and in `system`'s
`Controls`, `pyren-ctl network get`/`network set`, and the app wired end
to end — Tauri commands, `daemon.ts`, `hardware.svelte.ts`, and the
`system/network` page rebuilt around it (mode toggle, total bandwidth,
the interface/active-qdisc read-out, and a plain note that per-app
prioritisation is not available instead of a table with nothing behind
it). Confirmed on hardware, 2026-09-04: `network get` correctly finds `wlan0` as
the default-route interface and reads back its live qdisc (`noqueue`);
`network set auto` unprivileged correctly refuses with `permissionDenied`
naming the interface. As root: `network set auto` switched `wlan0` to
`cake` directly (this kernel has `sch_cake`, so the `fq_codel` fallback
was not exercised) — `tc qdisc show dev wlan0` confirms it took
(`qdisc cake 8001: root ...`) — and `network set off` reverted it cleanly
to `noqueue`, the interface's own default. Nothing left running or changed
afterward.

**GPU switching decided and built, 2026-09-04.** Not `supergfxctl` — the
driver this project already patches and installs for fan control
(`driver/hp-wmi-omen/hp-wmi.c`) turned out to expose
`/sys/devices/platform/hp-wmi/gpu_mux_mode` directly, talking
`HPWMI_GRAPHICS_MUX_QUERY` over ACPI-WMI with no third daemon in the way.
Confirmed present and readable on the development machine before anything
was written: `cat gpu_mux_mode` answered `0` (hybrid), matching the UI's
own default. Wrapping `supergfxctl` — not installed here, and now strictly
worse than the file already open — was never seriously in the running once
that was known.

Built: `daemon/crates/gpu` (`gpu.getStatus` / `gpu.setMode`, see
`docs/01-ipc-protocol.md` §"`gpu` module"), registered in the daemon and in
`system`'s `Controls` so the compatibility line picks it up, `pyren-ctl gpu
get`/`gpu set`, and the app wired end to end — Tauri commands, `daemon.ts`,
`hardware.svelte.ts` (`syncFromDaemon` now reads the real mode at startup;
`setGpuMode` writes it and surfaces a refusal instead of pretending the
write landed). `gpu.getStatus` confirmed on hardware, reading back
`hybrid`. **`gpu.setMode` has not been run** — it changes the session's
card and needs a logout or reboot, so it is one to try deliberately with
`pyren-ctl gpu set <mode>` rather than something this pass should have
fired blind.

---

## 3. Worth doing, not urgent

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
- **Confirming the fan cleaner against firmware that has it.** The feature
  is ported and the app has a page for it, but no machine here answers the
  capability query — so what byte 8 of the modern reply really means, and
  whether the legacy toggle is the right bit, are still upstream's word.
  `fan.startCleaning { "force": true }` exists for exactly this: it skips
  the "no fan cleaner here" refusal so a machine that has the feature can
  be tried against a build that decodes its answer wrongly.
- **Reinstall the driver with the measured fan ceiling.** §1.1 put a real
  number on this chassis — 5200 rpm, both fans — and the driver was
  installed before it existed, so it is still running on whatever ceiling
  the firmware volunteers. `--cpu-max-rpm 5200 --gpu-max-rpm 5200` on a
  reinstall would pin the measured one. Nothing is visibly wrong without
  it, which is why this is here and not in §1.
- **Packaging**: `tools/install.sh` covers the binaries and the widget's
  user unit, which is what §1.4 needed; a PKGBUILD is still the right next
  step, given the audience. It would also settle where the *daemon's*
  system unit comes from — nothing installs one today, and the service
  running here points at a debug build inside the tree.
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

- **No offset has ever been written, on any machine, and on this one none
  can be.** The two walls were found on the same afternoon and they are
  not the same wall. *As the user*, who does get into their own X server,
  both offset attributes read fine and a write comes back "the current
  user does not have permission" - which is what a screen with no
  `Coolbits` says, and root does not change it, because `Coolbits` is a
  property of the **X screen** rather than of the client. *As root*, the
  daemon does not even get that far: the server admits the uid that owns
  it and the compositor starts `Xwayland` with no `-auth` file, so there
  is no cookie for `PYREN_XAUTHORITY` to point at. A Wayland-only desktop
  therefore has nowhere to put `Coolbits` and no Xorg screen to put it on:
  **on this laptop the offsets are unreachable by design of the session**,
  and the module says so in those words rather than pretending. What is
  left to try, and needs different hardware or an Xorg session:
  `pyren-ctl oc probe --write` first, then a small `oc set --core 15`.
- ~~The clock lock has not been run as root.~~ **Done, and it works**:
  as root, 900-1200 MHz took the idle card from 180 MHz / P8 / 7.5 W to
  892 MHz / P5 / 9.9 W, and letting the confirmation lapse put it back on
  its own. The revert timer has now run against a real GPU, not only
  against refusals.
- ~~A root daemon cannot reach the offsets on a Wayland desktop.~~
  **Confirmed against the installed service**, and the reporting is fixed.
  `xhost +si:localuser:root` from inside the session is the only way to
  let the daemon in, and it is not installed here (`xorg-xhost`) - which
  changes nothing, since `Coolbits` would refuse the write afterwards
  anyway. Two things were wrong in how this was *reported* and both are
  now right: the startup probe of a systemd service runs at boot, before
  anybody has logged in, so it found the display manager's `:0` and called
  it "our own session"; it now names whose display it is, says a desktop
  was not running yet, and points at `overclock.probe`. The app asks for a
  fresh probe when the page is opened, by somebody who is by definition
  logged in.
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

- **The fan module does not re-discover its sysfs paths.** `FanModule`
  finds `pwm1` and friends once, in its constructor, so a driver installed
  while the daemon is running does not become usable until it restarts -
  the wizard tells the user to restart it. Making the module re-probe
  (a `refresh` on `fan.getStatus`, or a signal from the installer) would
  remove that step, and is worth doing if installing ever becomes common.
  It stayed out for now because re-probing on every status call is a
  syscall on a hot path to fix a once-per-install annoyance.
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

- **Four of §3 landed together, and one of them was already done.**
  2026-09-04. None needed the laptop, which is why they were the ones to
  do while the hardware items wait.

  - **A second reference sensor.** `FanConfig.referenceSensor` is `cpu` or
    `gpu`, `fan.setCurve` takes it, and the fan page offers it - but only
    where hwmon publishes a GPU sensor, which this machine does not. The
    fallback is one-directional and that is the design: `gpu` falls back
    to the CPU when the card reads nothing (0 C is a card that is asleep,
    not a cold one), `cpu` never falls back to the GPU, because a curve
    silently driven by the sensor the user did not pick is a machine
    nobody asked for. The status carries the setting *and* what is being
    read, since they differ exactly while the card sleeps.
  - **Temperature in the power supervisor.** A third rule beside the two
    power-source ones: over `tempHighC` the machine is held at the quiet
    end until it is back under `tempLowC`. Two thresholds rather than one
    - a single line would step down, watch the fans win back a degree,
    and step straight into the same wall again - and the latch survives a
    manual override, because how hot the machine is is not an opinion the
    user overrode. Heat outranks load for the reason the rule exists: a
    machine is hot *because* it is busy, so if load won it would never
    fire.
  - **Reaching Eco and Balanced tuning from the UI.** The power sub-tab
    keeps the reference app's placement (Performance and Unlimited only)
    and gains a selector for *which* profile the sliders edit, so an Eco
    envelope no longer needs `pyren-ctl`. The page says whether what is
    being tuned applies now or at the next switch; `power.setTuning`
    already took a `mode`, so nothing changed below the UI.
  - **Logging.** `pyren_core::log` - four levels, `PYREN_LOG`, no
    dependency. The ~20 prints in the modules went through it; the
    startup report, `--check` and `--help` deliberately did **not**, since
    those are output somebody asked for and `PYREN_LOG=warn` emptying a
    report would be a bug rather than a setting.
  - **Per-process GPU usage** was already there and the entry was stale:
    the daemon walks `/proc/*/fdinfo` for it (`DrmUsageReader`), the
    column renders it, and `--` means that process holds no DRM client.

  Two things found on the way, both fixed. Four `pyren-fan` tests were
  setting and then *removing* `PYREN_ACPI_CALL` in parallel threads, which
  was harmless for as long as this machine had no `/proc/acpi/call` - the
  fallback the removal exposed was also a path that did not exist. Now
  that `acpi_call` is loaded the same race reaches the real firmware
  interface and two of them fail; they hold one lock and restore the
  variable rather than deleting it. And the CPU/GPU sensor lookup, which
  the fan module owned, is now `pyren_core::sensors`, because the
  supervisor wanted the same two numbers for a different reason.

- **The board-params variant is decided, not guessed.** Reading `hp-wmi.c`
  answered the question the install raised: all four variants share one fan
  profile, and a board already on the OMEN or Victus thermal-profile path
  never has its variant's EC offset read - so on most boards, 8D2F
  included, the choice is **inert**, and `autodetect` now says that instead
  of offering a caveat about a decision with no effect. Where it is live,
  it is measured: `ec_sys` read-only, offsets 0x59 and 0x95, whichever
  holds a value the OMEN v1 profile uses. Deliberately no inference from
  the Victus S values 0x00/0x01 - two of the commonest bytes in EC space,
  so matching them would name an offset from noise.

  Two bugs found on the way, both invisible in the source. Six message
  literals had lost their `\` line continuations and were carrying the
  source indentation into the sentence ("in the driver's<18 spaces>{table}"),
  and repairing them without keeping a space in front of the backslash glues
  the words either side together. `pyren_core::msg`'s
  `no_message_carries_its_own_source_indentation` now scans every literal in
  `crates/` for both shapes; it knows a swallowed continuation from
  deliberate alignment by *position* - a word character on the left and the
  start of a word on the right - and skips lines using the `\x20` marker
  the CLI help text aligns with.

- **The patched driver is installed on 8D2F, and it worked** (was §1.1, and
  the first run of the installer's execution path). `FINDINGS.md` has the
  evidence. In short: automatic mode read the board out of DMI, added it to
  `hp_wmi_feature_boards` with `omen_v1_no_ec` params, built via the hooks
  strategy, and `pwm1`/`pwm2` now exist where the stock 7.2.2 driver
  produced neither. The inference in `FINDINGS.md` §"What the driver
  actually gates on the board table" was right: nothing gates the pwm path
  on anything but board-params being set at all.

  Three things the run taught, all now fixed: **the vendored tree really is
  left alone** (`git status` on `driver/` is clean after an install - the
  stage-before-patch order works); **an injected entry has to keep the
  table's indentation**, since inserting at the `{}` sentinel spliced it
  into that line; and **the daemon must be restarted afterwards**, because
  the fan module discovers its sysfs paths once at startup, so it goes on
  reporting "no pwm1" next to a `/sys` that has one. The wizard now says
  so, with the command.

- **The driver is vendored, and the installer works out its own inputs**
  (was §2.1, "vendor, submodule, or fetch?"). `driver/` is now a verbatim
  copy of upstream's tree; `FINDINGS.md` has the reversal and
  `driver/README.md` the provenance. What decided it: without the copy, the
  driver path did nothing on a fresh machine and the blocker told the user
  to go and clone something else - which is not a smaller cost than a
  manual sync, it is the feature not existing.

  On top of that, `installer.autodetect` reads the four answers the form
  used to ask for: the board id and model from DMI, whether the driver
  already lists that board from `hp_wmi_feature_boards` in its own source,
  and the fan ceilings from `fan.json`. Decisions worth keeping:
  **`stage-source` now runs before `patch-source`**, so patching writes to
  the copy under `/usr/src` and the vendored tree stays pristine - a second
  install must not start from the first one's output; **the params variant
  is presented as a choice, not a reading**, because DMI cannot say which
  EC offset a board uses, so the conservative variant of the right family
  is picked and the note says it is a guess; **a machine that is neither an
  OMEN nor a Victus gets nothing filled in**, since the two families write
  different thermal-profile values and a wrong default is worse than an
  empty field; and **an uncalibrated machine gets null ceilings**, letting
  the driver ask the firmware, rather than a number Pyren invented. The
  wizard offers automatic and manual as two modes rather than a button over
  a form, and neither will apply without a second, separate confirmation.

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
  It has since been **run against hardware, successfully** — see the
  entry at the top of this section.

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
  The `pwm1`-dependent modes correctly refused to run on this board while
  the stock driver was in place; installing the patched one gave it a
  `pwm1`, so what is left there is calibration, not code.
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
