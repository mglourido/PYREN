# TODO

Priority order within each section. Each item says what blocks it and how
to tell when it's done. Items marked **[HP]** cannot be verified on the
non-HP development desktop.

---

## 1. Do next

### 1.1 Restrict the daemon socket — security
The daemon runs as root and now *writes* (power modes, and fan control
later). `UnixListener::bind` in `daemon/crates/core/src/lib.rs` leaves the
socket at the default umask, so **any local user can drive it**. The design
plan already calls the socket the trust boundary; it isn't one yet.

Fix: `chmod` the socket to `0660` and own it by a group (`omen-hub`), or
put it in a mode-`0750` `/run/omen-hub` and add the desktop user to the
group. The systemd unit already uses `RuntimeDirectory=omen-hub`, so most
of the plumbing exists. Decide whether a non-member should get read-only
access rather than none.

*Done when*: a second local user cannot change the power mode.

### 1.2 Add a LICENSE file
Every `Cargo.toml` declares `GPL-3.0-or-later` and `app/package.json` says
MIT; the repository has neither file. Pick one, make them agree, add the
text. Note the frontend and daemon *could* legitimately differ, but right
now it looks like an oversight rather than a decision.

### 1.3 Settle the 8D2F question **[HP]**
Run `sudo sh omen-check.sh --json` on the laptop with the *current* script
and read the two new checks. See `FINDINGS.md` §"Board 8D2F" — this decides
whether the fan write path is worth building at all, so it comes before
1.4.

### 1.4 Fan write path: `fan.setMode` / `fan.setCurve` **[HP]**
The last thing standing between the app and actually controlling fans.
Blocked on 1.3 on this hardware, but the logic can be built and unit-tested
anywhere:

- curve interpolation already exists in the frontend
  (`curveValueAt`) and needs a daemon-side twin
- hysteresis so the fan doesn't oscillate at a threshold
- a control loop that re-applies on a timer — the EC reverts to its own
  curve ~120 s after being overridden, and the kernel driver's keep-alive
  is only every 90 s
- `pwm1_enable` semantics: 0 = max, 1 = manual, 2 = automatic
- persist the curve through `omen-hub-config` (the fan module has no config
  file yet; this is where the persistent/volatile split from the Python
  original earns its keep)

The behavioural spec is `docs/04-fan-control-logic.md` in the source
project. Do **not** copy its numbers blindly — see `FINDINGS.md` on its
documentation disagreeing with its own code.

### 1.5 Continuous integration
`cargo test`, `cargo clippy -- -D warnings`, `svelte-check`, `vite build`,
`cargo check` on `src-tauri`, and `sh -n tools/omen-check.sh`. The parity
test especially: it has caught three real bugs and nothing currently runs
it automatically.

---

## 2. Blocked on a decision or on hardware

### 2.1 RGB module **[HP]**
Blocked on one command on the laptop: `lsusb | grep -i 0d62`. Per-key USB
HID and the 4-zone ACPI lightbar share nothing, so that answer decides
which of two unrelated ports to write. Review and porting order are in
`docs/04-rgb-porting-review.md`.

Whichever path: `/proc/acpi/call` needs a **cross-module** lock (the fan
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

- **Structured IPC errors.** Everything is a string today, and
  `docs/01-ipc-protocol.md` already warns against string-matching them. Move
  to `{ kind, message }` before any caller needs to branch on kind.
- **Logging.** ~20 `println!`/`eprintln!` calls. A level-filtered logger
  would let the daemon be quiet under systemd and verbose when diagnosing.
- **Temperature in the power supervisor.** It decides on load and battery
  only; a hot chassis is a good reason to back off.
- **Fan cleaner** (reverse spin, `acpi_call`) — the protocol is documented
  in the source project, and it's the one genuinely novel feature.
- **Calibration**: spin to max, measure, store — needed for a meaningful
  0-100 % fan scale rather than raw PWM.
- **Import Windows OMEN config** (`PowerControlConfig.json`, gzip'd UTF-16
  JSON on the Windows partition) to skip calibration for dual-booters.
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
  for two sessions.

---

## 4. Deliberately not done

Recorded so nobody "fixes" them by accident:

- **No `core.json`.** Cross-cutting daemon config has no contents yet.
- **`system` always reports `supported: true`.** Any Linux machine can
  report its own vitals; hardware-*control* support is a different question,
  answered by `system.getInfo`'s `compatibility`.
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
