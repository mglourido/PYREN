# What works, and how we know

This is the honest version: what has been tested, how, and what has
**not** been. Every number here comes from a suite you can run yourself.

```sh
cd daemon && cargo test --workspace     # 500 tests
tools/power-soak.sh                     # 31 checks, against real hardware
```

## At a glance

| feature | works? | notes |
|---|---|---|
| **Power profiles** (Eco / Balanced / Performance / Unlimited) | ✅ **yes** | verified on hardware — moves both the firmware profile and the OS one |
| **Power limits** (PL1 / PL2), Performance & Unlimited | ✅ **yes** | verified on hardware — 45 W asked, 44 W applied, clamped to stock |
| **Turbo on/off** per profile | ✅ **yes** | verified on hardware |
| **Profile survives closing the app** | ✅ **yes** | the app is a client; the mode belongs to the daemon |
| **Profile survives a daemon restart** | ✅ **yes** | only with *restore on start* on — off by default, on purpose |
| **Automatic switching** (battery / load / heat) | ✅ **yes** | soaked; one switch per half hour under load, no flapping |
| **Fan modes** auto / max / curve | ✅ **yes** | tested against a fixture |
| **Fan mode** manual | ⚠️ **partly** | tested lightly — accepted where the driver exposes `pwm1`, refused where not |
| **One fan curve per profile** | ❌ **does not exist** | there is a single global curve; the fan module does not know the power mode. Would be a new feature |
| **GPU overclocking** | ❌ **not on this machine** | the code works and has 44 tests, but the offsets cannot be applied here — see below |

### Why GPU overclocking does not work here

NVIDIA offers two ways to move a clock offset, and this machine can use
neither *as Pyren currently asks for it*:

| path | needs | status here |
|---|---|---|
| `nvidia-settings` (what Pyren uses today) | a real Xorg server with an NVIDIA X screen, and `Coolbits` enabled | ❌ this is a **Wayland** session (Hyprland + rootless Xwayland). `nvidia-settings -q screens` finds no NVIDIA screen at all, so there is nothing for `Coolbits` to apply to |
| **NVML** (`libnvidia-ml`) | nothing but root | ✅ **available** — driver 610.57.04 exposes `nvmlDeviceSetGpcClkVfOffset`, and the offset reads back fine with a −1000…+1000 range |

So the blocker is not a permission the user has failed to grant. It is
that the mechanism Pyren reaches for does not exist on a Wayland desktop,
while one that does exist is not wired up yet. Enabling `Coolbits` would
change nothing here; it would only help somebody running a real Xorg
session.

---

Two kinds of evidence appear below, and they are not equally strong:

| | what it means |
|---|---|
| **Tested** | a suite asserts it against a fixture — a fake machine built out of temporary files, so the assertions are exact and run anywhere |
| **Verified on hardware** | it was also run against a real laptop and the kernel's own sysfs was read back to confirm |

A fixture can prove the code does what it intends. Only hardware can
prove the intention was right about the machine. Where the two disagree,
the hardware wins, and twice on this page it did.

The reference machine is an **HP OMEN (board 8D2F)**: Intel Arrow Lake-P,
RTX 5060 Mobile, 77 W package limit, `cool`/`balanced`/`performance`
firmware profiles, running CachyOS with Hyprland and
power-profiles-daemon 0.30.

---

## Power profiles — Eco, Balanced, Performance, Unlimited

**Verified on hardware.** Picking a mode moves two separate things: the
laptop's own ACPI profile (which is what changes the EC's fan curve) and
the OS profile that your desktop's battery menu shows.

| mode | firmware profile | OS profile |
|---|---|---|
| Eco | `cool` | `power-saver` |
| Balanced | `balanced` | `balanced` |
| Performance | `performance` | `performance` |
| Unlimited | `performance` | `performance` |

Performance and Unlimited land on the same two profiles on purpose:
there is no firmware profile above `performance`, and what makes
Unlimited different is the power envelope, not a fifth name no firmware
has.

Also tested: cycling all four for twenty rounds without drift, the
performance key stepping through them, ten threads switching at once,
turning the OS half off so only the firmware profile moves, and a
machine with no mechanism at all reporting an error instead of moving a
highlight over hardware that did not change.

## Energy settings — power limits and turbo

**Verified on hardware.** These are the watts, and they are offered for
Performance and Unlimited.

```
Performance, asked for PL1 45 W / PL2 60 W  →  44 W / 60 W applied
Unlimited, untuned                          →  77 W / 77 W (stock)
back to Performance                         →  44 W / 60 W re-applied
turbo off on Performance                    →  no_turbo=1
Unlimited                                   →  no_turbo=0
```

The 45 that comes back as 44 is not a rounding bug being papered over:
limits are stored as a whole percentage of *this* machine's ceiling so a
saved config means the same thing on different hardware, and one percent
of 77 W is 0.77 W.

Also tested: no mode can be tuned above what the firmware shipped
(that would be overclocking the CPU, which is a separate feature with
separate consent), an absurdly low limit is floored at something the
machine can still respond at, tuning one mode does not touch the other
three, editing the mode you are currently in applies immediately, and a
tuned envelope survives a daemon restart to the microwatt.

## Fan control

**Tested** against a fixture; not driven at length on real fans on
purpose.

| mode | what it does | status |
|---|---|---|
| `auto` | hands the fans back to the firmware's own curve | tested |
| `max` | full speed | tested |
| `curve` | follows your temperature → speed curve | tested |
| `manual` | one fixed speed | tested lightly — see below |

The curve is tested end to end: the stored curve, the machine's
temperature, and the PWM that actually reached the hardware, with the
expected value computed through the module's own public curve function
rather than hard-coded. Editing a curve while it is running moves the
fans at once rather than at the next tick.

`manual` gets one test and no more. It pins the fans at a speed nobody
is watching, which is not a state to leave a laptop sitting in during a
test run. What matters about it is that it is accepted where the driver
exposes `pwm1` and refused where it does not — one assertion each. A
fixture without `pwm1` (which is what board 8D2F's stock driver looks
like) covers the refusal, and still does `auto` and `max`.

**There is one fan curve, not one per profile.** Switching from Eco to
Unlimited does not change your curve — the fan module has no idea what
power mode you are in, by design. The app only *shows* the curve editor
in Unlimited; that is a decision in the interface, not in the daemon.
Per-profile curves would be a new feature, not a setting that exists.

## Closing the app, and restarting the daemon

**Verified on hardware.** These are two different events and only one of
them can lose anything.

| event | what happens to your profile |
|---|---|
| you close the app — foreground, background or minimised to the tray | **nothing.** The app is a client; the mode belongs to the daemon, which is still running |
| the daemon restarts (reboot, or `systemctl restart`) | the mode in memory is gone. It comes back only if **restore on start** is on; otherwise the daemon reports whatever the firmware is actually set to, and changes nothing |

The supervisor keeps making decisions with nobody connected — that is
the whole reason it is a daemon and not a thread inside a window.

## Automatic switching

**Tested** over simulated timelines, and soaked for real.

- Half an hour under sustained load produces **exactly one** switch. A
  mode change spins the fans audibly, so a supervisor that kept making
  them would be worse than one that never acted.
- Twenty minutes of load hovering between the thresholds produces
  **none** — that gap is a deliberate dead band.
- A realistic forty-minute afternoon (idle → a build → the chassis heats
  up → unplugged → the battery runs down) ends in Eco having made at
  most four switches, and never reaches Unlimited: that one is yours to
  pick, never the daemon's.
- Unplugging is answered within one tick, even while a manual choice is
  otherwise keeping the supervisor quiet — pulling the cable is you
  speaking too, and more recently.

A real-clock soak of the actual supervisor thread is also available and
was run: 60 seconds, one switch to Eco at 7 s, then held.

```sh
PYREN_SOAK_SECS=300 cargo test -p pyren-power --test profiles -- --ignored
```

## Module boundaries

**Tested.** Three modules touch power-adjacent hardware and none of them
calls the others. The bug this guards against is not a wrong value, it
is a value that depends on which module wrote last.

- Changing the power mode never writes to the fans. A lower power limit
  makes them spin less because there is less heat, which is the honest
  way to get there.
- Changing the fan mode never writes to the power envelope.
- An overclock request never reaches the CPU's power limits.
- Twelve mode changes and six fan changes later, the overclock consent
  is still on file. The three modules write three separate config files.

---

## Two real bugs this found

Both were found by running against real hardware, not by the fixtures,
and both are fixed with regression tests.

**1. power-profiles-daemon was overwriting the firmware profile.**
Version 0.30 ships its own `platform_profile` driver, so handing it the
OS profile could silently overwrite the firmware profile Pyren had just
set — Eco landing on `balanced` instead of `cool`. Pyren now applies the
OS profile *first* and its own firmware write last, so its write is the
one that stands.

**2. power-profiles-daemon does not always apply what it is asked for.**
Reproduced with Pyren entirely out of the picture: going straight from
`performance` to `power-saver` reproducibly settles on `balanced`. Pyren
now reads back what actually took effect and asks a second time before
giving up, and reports a failure rather than trusting an exit code.
Verified: the sequence that failed every time now passes 5/5.

---

## What is *not* tested, and why

**GPU overclocking has never been run against a real card here.** The
module is implemented and its logic has 44 tests of its own — the
consent gate, the stepped climb, the revert-on-failure timer, the refusal
to restore an offset after a crash. What has not happened is an offset
landing on real silicon, and that is deliberate: an offset that survives
a benchmark can still hang the machine in a game, and when it does there
is no error message.

On the reference machine it also **cannot** work as Pyren currently asks
for it, and the reason is worth being precise about, because the obvious
diagnosis is the wrong one.

Pyren applies offsets through `nvidia-settings`, which needs an X screen
driven by the NVIDIA X driver, with `Coolbits` enabled on it. What was
actually found:

1. **There is no NVIDIA X screen to enable anything on.** This is a
   Wayland session, and `nvidia-settings -q screens` returns nothing.
   Writing a `Coolbits` line into `xorg.conf.d` would be inert — it
   configures an Xorg screen that this desktop never creates.
2. **The write is refused for everyone, not just root.** Asked directly
   from the user's own session, `nvidia-settings -a ...Offset...=50`
   answers `Operation not permitted for the current user`. That is the
   same wall, seen from the other side.
3. **The daemon also has no X cookie**, since Hyprland's Xwayland was
   started without an auth file. Pyren diagnoses *this* one correctly and
   says so in its own error message, which is the behaviour that was
   tested.

None of that is a defect in Pyren. But it does mean the feature reaches
for a mechanism that a modern Wayland desktop does not have — while the
driver on the same machine exposes one that needs no X at all. NVML
(`libnvidia-ml.so.1`) carries `nvmlDeviceSetGpcClkVfOffset` and friends
on driver 610.57.04, the current offset reads back as 0, and the driver
advertises a −1000…+1000 range. Wiring that up is what would make
overclocking work here; a permission toggle would not.

Also not covered: RGB lighting, the keyboard remapper, the network
module and the driver installer are exercised by their own crates'
tests (37, 13, 13 and 84 respectively) but have not been through a
hardware pass like the one above.

---

## The numbers

| crate | tests | what it covers |
|---|---|---|
| `pyren-installer` | 84 | the driver installer and its plans |
| `pyren-power` | 77 | the four profiles, the envelope, auto-switching |
| `pyren-fan` | 75 | fan modes, curves, calibration, the cleaner |
| `pyren-overclock` | 44 | consent, the climb, the revert timer |
| `pyren-core` | 43 | IPC, config, sensors, the event bus |
| `pyren-rgb` | 37 | lighting zones and dialects |
| `pyren-hotkey` | 28 | the performance key |
| `pyren-system` | 26 | identity, metrics, compatibility |
| `pyren-daemon` | 20 | the cross-module boundaries |
| `pyren-network` | 13 | traffic shaping |
| `pyren-keymap` | 13 | key remapping |
| `pyren-check` | 12 | the self-test, and its shell twin |
| `pyren-config` | 10 | atomic writes, versioning, recovery |
| `pyren-ctl` | 9 | the command-line client's parsing |
| `pyren-gpu` | 9 | the graphics mux |

**500 total, 0 failing.** Plus 31 checks against real hardware in
`tools/power-soak.sh`, and one real-clock soak that is skipped by
default because it runs for minutes.

Everything is run on every push by
[CI](.github/workflows/ci.yml), along with
`cargo clippy --all-targets -- -D warnings`.
