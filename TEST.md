# What works, and how we know

This is the honest version: what has been tested, how, and what has
**not** been. Every number here comes from a suite you can run yourself.

```sh
cd daemon && cargo test --workspace     # 514 tests
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
| **GPU overclocking** | ✅ **yes** | verified on hardware — +50 MHz applied and reverted, through NVML. Needs no X and no `Coolbits` |
| **Reverting an unconfirmed offset on a reported fault** | ⚠️ **partly** | the decision logic is tested and the driver registration is verified on hardware; **no real fault has ever been seen firing** — see below |

### How GPU overclocking works here

NVIDIA offers two ways to move a clock offset, and only one of them
exists on a Wayland desktop:

| path | needs | status here |
|---|---|---|
| `nvidia-settings` | a real Xorg server with an NVIDIA X screen, and `Coolbits` enabled | ❌ this is a **Wayland** session. `nvidia-settings -q screens` finds no NVIDIA screen, so there is nothing for `Coolbits` to apply to — the write is refused for the user as much as for root |
| **NVML** (`libnvidia-ml`) | nothing but root | ✅ **used** — Pyren tries this first, and `nvidia-settings` only when the driver is too old to have it |

This was the fix. Pyren originally reached for `nvidia-settings` only,
which is why overclocking appeared not to work at all: it was asking for
a mechanism a Wayland desktop does not have. Verified end to end
afterwards:

```
offset before      0 MHz
apply +50 MHz  →  50 MHz on the card, revert timer armed
cancel         →   0 MHz
```

Read back through NVML directly rather than taken from Pyren's own
report, and the card was left at stock.

### Reverting when the card complains, not only when the timer runs out

The watchdog that undoes an offset nobody confirmed now has a second
reason to act: the driver reporting that the card faulted. Both go
through one pure function, `watchdog_tick`, which is what makes the
decision testable without a thread or a GPU.

**Tested.** Eight tests in `pyren-overclock` cover the whole decision:
nothing armed reverts nothing however the card is behaving; a
change still inside its hold on a healthy card is left alone; a hold that
runs out is `NotConfirmed`, as it always was; a fault inside the hold is
`FaultReported`; and when both are true in the same 500 ms tick the
deadline wins, because "you never confirmed this" is the older and surer
of the two facts. A revert also disarms what it undid — even the failing
revert of a card that has gone away — so the tick half a second later
finds nothing left to undo and the card is never written to twice. The
two notes are asserted to differ, in key and in words, and to name the
card: a user reading `note` afterwards has to be able to tell "you took
too long to click" apart from "the card actually complained".

**Verified on hardware**, three checks, all non-destructive:

```
NVML present                                   yes
gpu0 advertises nvmlEventTypeXidCriticalError   yes  (bitmask 0xf19c, bit 0x8)
a poll on a healthy card                       no event, 1.08 ms
```

The poll waits 1 ms, not the 400 ms the plan first suggested: the driver
queues events into the set as they happen, so a poll finds whatever
already arrived without waiting for it, and a longer wait would only push
the watchdog's own 500 ms deadline check late. The measured 1.08 ms is
asserted in the suite to stay under 100 ms — and that assertion, like
every other one in `nvml.rs`, still passes on a machine with no NVIDIA
driver at all, where it simply has no watch to poll.

**What this does not mean.** Nobody has watched a real XID fire and be
caught. The plan that introduced this said so before it was built and it
is still true: registering for `nvmlEventTypeXidCriticalError` and
confirming the GPU's bitmask advertises it is *"not the same as having
watched a real XID fire and be caught"*, and a "done" here means *"the
three live checks above passed, and that the fixture-level decision logic
has tests"* — nothing more. There is no safe way to manufacture a
critical GPU fault on demand, and corrupting a card on purpose to test
the corruption handler is not a trade this project makes. The convincing
evidence *"will only ever come from an offset search that goes wrong by
accident during ordinary use after this ships, not from a test suite
manufacturing the failure on purpose"*. A card that does not advertise
the bit, or a driver too old to have the event API, gets exactly the
behaviour that was there before: the timer alone, silently.

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

**GPU overclocking is now verified on hardware, but only shallowly.** A
+50 MHz core offset was applied, read back off the card and reverted. The
module's safety machinery — the consent gate, the stepped climb, the
revert-on-failure timer, the refusal to restore an offset after a crash —
has 58 tests of its own, and the revert path was exercised live.

What has *not* been done is finding out how far this card will actually
go. That is not a test, it is an afternoon with a workload: an offset
that survives a benchmark can still hang the machine in a game, and when
it does there is no error message. Pyren applies offsets in 15 MHz steps
and reverts anything you do not confirm, which bounds the damage; it
cannot tell you your card is stable.

Memory offsets are **verified on hardware** too. A `+400` offset moved
the reported memory clock from 12001 to 12201 MHz — half the offset,
which is exactly the relationship the module documents: the driver
advertises −2000…+6000 here, and a memory offset is a *transfer rate*
offset, so what other tools show as "memory clock" is half of it. Read
back through NVML and reverted with `cancel`.

**On battery, an offset applies and does very little.** The power source
does not change whether the write lands — a live `+200` moved the core
ceiling from 3090 to 3285 MHz on battery — but `nvidia-smi -q -d POWER`
reported the card's current ceiling at 50 W against an 80 W default, a
limit the driver sets on its own and that neither `pyren-power` nor
`pyren-overclock` touches. A card that hits 50 W before it hits the new
clock ceiling gets nothing out of the offset until the machine is
plugged in. Not a bug in either module; worth knowing before spending an
evening on "why did the offset do nothing".

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
| `pyren-overclock` | 58 | consent, the climb, the revert timer and the fault revert, the NVML binding |
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

**514 total, 0 failing.** Plus 31 checks against real hardware in
`tools/power-soak.sh`, and one real-clock soak that is skipped by
default because it runs for minutes.

Everything is run on every push by
[CI](.github/workflows/ci.yml), along with
`cargo clippy --all-targets -- -D warnings`.
