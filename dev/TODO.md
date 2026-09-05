# TODO

Priority order within each section. Each item says what blocks it. Items
marked **[HP]** need the HP laptop — which *is* the development machine as
of 2026-09-02, so what they now need is root on it, not different hardware.

Almost everything that was open when this file grew past a page is now
done. The postmortems that used to live here — what broke, what the trace
looked like, which decision turned out to matter — are in `FINDINGS.md`
and in `git log`; this file only tracks what is still open.

---

## 1. Open

### Key mapping: not yet run against hardware **[HP]**
The evdev remapper (`daemon/crates/keymap`, `keymap.*`, `pyren-ctl keymap`,
`/system/keys`) is built and unit-tested, but grabbing *this* development
machine's own keyboard from inside the session used to edit the code is
not a test to fire blind — a wrong substitution takes the keyboard away
from whoever needs to fix it. `pyren-ctl keymap on` on a spare keyboard, or
over SSH with a way back in, is the next honest step. What it would
confirm: the grab and virtual device come up, ungrab-on-disable hands the
keyboard back, and ordinary typing survives a mapping that exists but is
switched off.

### GPU switching: the reboot swap has never been watched **[HP]**
`gpu.setMode` writes `gpu_mux_mode` and reads it back correctly (confirmed
on hardware), but the mode only takes effect at the next logout or reboot,
and nobody has sat through one to see the driving card actually change.
Low urgency — the write path is proven — but it is the one thing about
this feature still untested.

### Fan cleaner: ported, never asked of real firmware
The feature is ported and the app has a page for it. This used to be
blocked on `acpi_call`; it is installed and loaded now — it is what
drives the lighting — so the capability query can finally be put to this
machine's firmware. Nobody has recorded doing it, and if board 8D2F turns
out to have no fan cleaner the feature still needs a machine that does.
Until then, what stays upstream's word: what byte 8 of the modern reply
means, and whether the legacy toggle is the right bit.
`fan.startCleaning { "force": true }` skips the "no fan cleaner here"
refusal so this can be tried on a machine that has the feature.

### AMD Overdrive is detected and not driven
`pp_od_clk_voltage` is a two-line write away and stays unwritten until
there is an AMD machine to test on: a wrong value there does not fail with
an error message. The probe already says so.

### Raising CPU PL1/PL2 above stock
Deliberately not in the `overclock` module yet: the `power` module owns
those registers and re-applies them, clamped to stock, on every mode
change. Doing it means giving one module ownership of the envelope, which
is a decision, not an addition — and it would need the same consent gate
`overclock` already has for GPU offsets.

---

## 2. Worth doing, not urgent

- **Import the Windows OMEN profile.** `PowerControlConfig.json` on the
  Windows partition (gzip'd UTF-16 JSON) holds what HP itself considers
  this chassis's Eco and Performance — the one honest source of
  per-machine envelope numbers that exists, worth more than any default
  this project could invent.
- **PCIe ASPM policy as an explicit setting.** On a board with no firmware
  profile, changing `platform_profile` reaches nothing — the envelope is
  all there is. Exposing `/sys/module/pcie_aspm/parameters/policy`,
  off-by-default, would recover a little of what a firmware profile would
  have moved. Not something to switch on anyone's behalf, and not a fan
  curve.
- **Packaging.** Done for the first release: `tools/release.sh` builds the
  archive, `install/install.sh` sets a machine up (both units, the `pyren`
  group, the driver sources under `/usr/share/pyren`), `install/INSTALL.md`
  is the process. A distro package (`.deb`, a PKGBUILD) is deliberately not
  built. Still local-only: the *dev* machine's `pyren-daemon.service` points
  at a debug build inside the tree, which `tools/dev-all.sh` restarts; a
  real install replaces it.
- **`kernelZones` needs a module nothing installs.** The RGB dialect that
  reads all four zones back and never hand-builds a firmware buffer is
  `kernelZones`, over the out-of-tree `OmenLinux/omen-rgb-keyboard` module.
  `tools/try-kernel-zones.sh` installs it by hand (and rolls back if
  `hp-wmi`'s `pwm1` stops answering); it is not persistent across a kernel
  upgrade and the installer does not carry it. Until it does, a fresh
  machine auto-picks `fourZone`, whose fourth zone is written correctly but
  reads back black because `acpi_call` truncates the reply. Either package
  the module (DKMS, alongside the driver install) or accept the `fourZone`
  read limit as documented. See `FINDINGS.md` §"`kernelZones` works".
- **The per-key RGB keyboard path is not ported.** Only the 4-zone lightbar
  is. Port the USB-HID path (`0d62:54bf`, HP Gaming Keyboard II) only if
  such a device turns up on a machine — this one has none (`FINDINGS.md`
  §"The test laptop has no per-key RGB keyboard"). Settle first the
  `set_all()` vs `keys.json` contradiction about `backspace` (`FINDINGS.md`
  §"`keys.json` really does contradict `set_all()`"): set the keyboard
  white, look at backspace, then `set-key backspace ffffff` — a dark
  segment in the first case but not the second means `set_all()` is
  blanking real LEDs; identical both times means the key-map entry is the
  wrong shape. Three upstream quirks to carry over: the HID lighting
  interface is write-only (`get_colors()` returns the driver's buffer, not
  hardware state), the interface-number-3 filter in `hid.enumerate` tells
  the lighting endpoint from the input ones, and the double-write of the
  `0x0a` commit report is marked mandatory — keep it.
- **End-to-end IPC test**: spawn the daemon on a temp socket, exercise
  every module method. Would have caught the Tauri command wiring being
  untested for two sessions, and is the natural place to prove the
  socket's permissions from the outside — the unit tests assert the mode
  bits, but only a second user can prove the *effect*, and CI has none.
- **`shellcheck` in CI.** The workflow only runs `sh -n
  tools/pyren-check.sh`, which catches syntax and nothing else.
  `shellcheck` wasn't added because it isn't installed here and a lint
  nobody has run locally would land red.
- **The `nvidia-settings` offset fallback is unexercised.** GPU core and
  memory offsets are verified on hardware through NVML (`TEST.md`), which
  needs no X and no `Coolbits`; the `nvidia-settings` path only runs on a
  driver too old for NVML's offset support and has never been hit on any
  machine. `oc probe --write` on an Xorg session with an old driver is
  what would confirm it — not urgent, since the mechanism that matters
  works.

---

## 3. Deliberately not done

Recorded so nobody "fixes" them by accident:

- **The GPU-fault watchdog watches only for critical XIDs.** Not ECC error
  counters (this consumer GPU's bitmask marks single/double-bit ECC
  unsupported, and a card with no ECC memory has nothing to count), and not
  clock/thermal throttle reasons — a card throttling under an overclock is
  usually working as intended, so reverting on it would undo overclocks
  that were never dangerous. Throttle reasons are worth *showing* in
  `status()` some day, just not as a revert trigger. A general NVML event
  feed for the app is also out of scope: the watchdog protects the confirm
  window, it is not a monitoring feature.

- **No read-only access for non-members of `pyren`.** The socket is
  `0660`; a user outside the group gets nothing, not "vitals but no
  writes". Splitting reads from writes would mean opening a root daemon's
  socket to every local process — sandboxed and compromised ones included
  — to save an admin one `usermod -aG`. The protocol also cannot tell two
  group members apart, so no future method may depend on *which* one
  called.
- **No mode ships power-limit numbers.** Every laptop has its own internal
  profiles and their curves are not each other's, so a percentage that is
  a sensible Eco on one chassis is a throttled mess on the next. Out of
  the box a mode drives only the mechanisms the machine provides; the
  envelope is left where the firmware set it until someone puts in a
  number they measured.
- **A power profile never exceeds the firmware's own limits.** Eco caps,
  Performance and Unlimited restore; neither raises. "Unlimited" means
  this daemon imposes no limit of its own, not that the machine is
  unlocked.
- **The installer creates the `pyren` group but never deletes it.**
  Removing a group that users are still members of is not the installer's
  call, and a leftover empty group costs nothing.
- **The daemon does not touch the fans until asked.** At startup it
  reports the mode the hardware is *in* rather than the one in its
  config, and writes nothing. `restoreModeOnStart` is the opt-in, off by
  default.
- **`auto` is written once, not re-asserted.** In auto the firmware owns
  the fans; re-issuing it every minute would be a WMI call that changes
  nothing. Max, manual and curve are re-asserted every 60 s.
- **`system` always reports `supported: true`** as a *module*. Any Linux
  machine can report its own vitals; hardware-*control* support is a
  different question, answered by `system.getInfo`'s `controls`.
- **No board allowlist anywhere.** What a machine can do is probed, never
  looked up. If something ever needs a board id again, it must be advice
  attached to a *probe result*, not a substitute for probing.
- **The installer refuses to run when fan control already works**, unless
  forced. Replacing a working stock driver is a downgrade.
- **`apply` is a dry run unless `confirm: true`.** A mis-sent IPC message
  must not be able to replace a kernel module.
- **localStorage is a cache, not a record.** It exists so the first frame
  has the user's language; the file on disk always wins.
