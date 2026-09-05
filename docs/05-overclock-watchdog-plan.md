# Plan: reacting to a failing GPU, not just to an unconfirmed timer

**Status: proposed, not started.** Nothing in this file has been built.
It exists so the idea survives past the session that found it, and so
whoever picks it up does not have to re-derive where the hooks are.

## Where this came from

`TEST.md` at the root of the repo lists what the overclock module
has verified on hardware and flags one thing it has not: whether an
offset is *stable*, as opposed to merely applied. Two adjacent findings
from the same session narrow that gap further and are worth recording
here rather than only in chat history:

- **Memory offsets are now verified too.** `+400` on
  `GPUMemoryTransferRateOffsetAllPerformanceLevels` moved the reported
  memory clock from 12001 to 12201 MHz — half the offset, which is the
  transfer-rate-vs-clock relationship the module's own doc comment
  already warned about (`crates/overclock/src/nvidia.rs`, `MEM_ATTRIBUTE`).
  Read back through NVML directly and reverted with `cancel`. This closes
  what `TEST.md` listed as "implemented and untested on hardware" — that
  line should be updated when this plan's first checkbox is not the only
  thing that changes.
- **The power source changes what an offset can do, not whether it is
  applied.** On battery, `nvidia-smi -q -d POWER` reported the GPU's
  *current* power ceiling at 50 W against an 80 W default — a limit
  `power-profiles-daemon`/the driver sets on its own, outside anything
  `pyren-power` or `pyren-overclock` touch. The offset still writes and
  still moves the reported max clock (confirmed: core ceiling moved
  3090 → 3285 MHz with a live `+200` offset on battery), but a card
  capped at 50 W hits its power limit before it hits the higher clock
  ceiling, so the offset has little to nothing to do under real load
  until the machine is plugged in. Not a bug in either module — the two
  are legitimately independent — but worth a line in the overclock
  consent text or `TEST.md` so nobody spends an evening chasing "why did
  the offset do nothing" when the answer is "you were on battery."

Neither of those needs a design doc; they are done, or they are a
sentence to add somewhere. The rest of this file is about the one gap
that is not a quick fix.

## The gap: an offset can be wrong for longer than the timer needs to notice

`crates/overclock/src/lib.rs` already has a watchdog
(`OverclockModule::spawn_watchdog`, a thread on `WATCHDOG_TICK` = 500 ms)
that reverts a `Pending` offset once `hold_secs` runs out without a
confirm. That is the safety net for "the desktop died and nobody is
there to click anything" — the case the module's own doc comment
(`dev/TODO.md` §4, and the crate's module doc) calls out by name.

What it does not do is notice *early*. If the card starts reporting a
critical fault at second 2 of a 20-second hold, this daemon finds out at
second 20, the same as if nothing had gone wrong — sitting on a bad clock
for up to `hold_secs` (5–300 s, `MIN_HOLD_SECS`/`MAX_HOLD_SECS`) longer
than it has to. On a desktop that is still alive, that is 5–300 seconds
of instability the daemon could have cut short.

NVML can report the fault directly, and the reference GPU supports it:

```
nvmlDeviceGetSupportedEventTypes → 0xf19c
  bit 3 (0x8), nvmlEventTypeXidCriticalError: supported
```

An XID is the driver's own "this GPU just did something it should not
have" signal — the same mechanism `nvidia-bug-report` and every serious
NVIDIA monitoring tool reads. Confirming it is *supported* is not the
same as having watched one fire; see "What this plan does not claim" below.

## Design

Extend the existing watchdog rather than add a second one. It already
polls `state.pending` every 500 ms; the change is to also ask NVML, on
the same tick, whether the armed GPU has raised a critical event since
the last check — and revert immediately if so, using the exact same
`revert()` path the timeout uses today, with a new `RevertReason`.

```
                    ┌─────────────────────────────┐
   apply() arms     │  spawn_watchdog(), one tick  │
   Pending{ gpu,     │  per WATCHDOG_TICK (500ms)   │
   revert_to,        │                              │
   deadline }  ─────▶│  1. pending.deadline passed? │──▶ revert(NotConfirmed)
                    │     yes → revert, as today   │
                    │  2. no → any GPU with a       │
                    │     registered event set:     │
                    │     XID fired since last      │──▶ revert(FaultReported)
                    │     check?                    │
                    │  3. neither → sleep, loop      │
                    └─────────────────────────────┘
```

`FaultReported` is a new `RevertReason` variant alongside `NotConfirmed`
and `Undone` (`crates/overclock/src/lib.rs:784`), with its own message —
"the change to {gpu} was reverted: the driver reported a fault" — because
a user reading `lastNote` after the fact needs to tell "you took too long
to click confirm" apart from "the card actually complained."

## Implementation steps

1. **NVML bindings for the event API**, in `crates/overclock/src/nvml.rs`
   alongside the existing offset symbols (`get_core`/`set_core`/etc. —
   same `sym::<T>()` loader, same "a missing symbol is `None`, not a
   panic" contract that already exists there):
   - `nvmlEventSetCreate` / `nvmlEventSetFree`
   - `nvmlDeviceRegisterEvents` (mask = `nvmlEventTypeXidCriticalError`
     only — this plan is about a card that is actively failing, not a
     general telemetry feed; see "Deliberately excluded" below)
   - `nvmlEventSetWait_v2` (short timeout, e.g. 400 ms, so the call
     returns in time for the watchdog's own 500 ms tick rather than
     blocking the thread past it)
   - `nvmlDeviceGetSupportedEventTypes`, to gate the whole feature: a GPU
     or driver that does not advertise `XidCriticalError` in its bitmask
     falls back to exactly today's behaviour, silently. No error, no
     degraded-mode message — a card without this capability is not
     broken, it just does not have this one extra signal.
   - Wrap the four in a small `EventWatch` type (`create` on arm,
     `poll() -> bool` per tick, `Drop` frees the set) so
     `spawn_watchdog` does not touch raw NVML handles directly — matching
     how `Symbols`/`sym::<T>` already keep the unsafe FFI edge in one
     place in that file.

2. **Wire it into `apply()`**: when a `Pending` is armed
   (`crates/overclock/src/lib.rs:477`), also create an `EventWatch` for
   that GPU if `nvml::available()` and the GPU's bitmask has the XID bit;
   store it next to `Pending` (or in the `State` alongside it — `Pending`
   itself is `Clone`d into the watchdog loop today, so the watch likely
   wants to live outside it, e.g. `Option<EventWatch>` on `State`,
   created in `apply()` and cleared wherever `pending` is cleared: both
   `confirm()` and the two `revert()` call sites).

3. **Extend `spawn_watchdog`'s loop body** (`lib.rs:298-320`): after the
   existing deadline check and before `continue`, poll the watch (if one
   exists for the current pending GPU) and call
   `revert(&state, &probe, &store, pending, RevertReason::FaultReported)`
   on a hit, exactly as the deadline branch does today.

4. **Add `RevertReason::FaultReported`** (`lib.rs:784`) with its message,
   translated like its two siblings.

5. **Surface it in `status()`**: `lastNote` already carries the revert
   reason as a `Msg`; no new field should be needed, but check the app's
   overclock panel renders an unprompted revert note distinctly from a
   plain timeout (it may already, since both go through the same field).

## Testing plan

The honest split, matching how this crate is already tested
(`crates/overclock/src/nvml.rs`'s own tests are careful to pass on a
machine with no NVIDIA driver at all):

- **Fixture-testable:** the decision logic — "given a deadline and an
  event-watch result, which `RevertReason` fires, and does exactly one
  revert happen, not zero and not two." This is the same shape as the
  existing `spawn_watchdog`/`Pending` logic, which is *not* currently
  unit-tested in isolation from real GPUs either (it is a thread reading
  real state) — so this plan should also pull the "what to do this tick"
  decision out of `spawn_watchdog` into a small pure function
  (`fn watchdog_tick(pending: Option<&Pending>, fault: bool, now: Instant) -> Option<RevertReason>`)
  that *is* unit-testable without a thread or a GPU, and have both the
  deadline and the fault check go through it. That refactor pays for
  itself independently of this feature.
- **NVML symbol loading:** `available()`-style tests already in
  `nvml.rs` — "does this not panic on a machine with no driver" — extend
  the same way for `EventWatch::create`.
- **Cannot be fixture-tested, and should not be worked around:** whether
  a *real* XID actually arrives and actually gets noticed. There is no
  safe way to manufacture a critical GPU fault on demand — that is
  destructive by definition, and "corrupt this GPU a little, on purpose,
  to test the corruption handler" is not a trade this project should
  make. What can be verified live, cheaply and non-destructively:
  1. `nvmlEventSetCreate`/`nvmlDeviceRegisterEvents` succeed on the
     reference GPU (already confirmed: bitmask `0xf19c` includes the XID
     bit).
  2. `nvmlEventSetWait_v2` with a short timeout returns promptly with "no
     event" under normal operation, so the watchdog tick is not blocked
     by it — a live latency check, not a correctness one.
  3. The watchdog's *existing* timeout path continues to work exactly as
     it does today (already covered by this crate's own tests) — i.e.
     this change must be provably additive, not a rewrite of the path
     that already has hardware confidence behind it.

## What this plan does not claim

Registering for `nvmlEventTypeXidCriticalError` and confirming the GPU's
bitmask advertises it is **not** the same as having watched a real XID
fire and be caught. Nobody should read a future "done" on this plan as
"this was tested by crashing the GPU" — it should mean the three live
checks above passed, and that the fixture-level decision logic has tests.
The genuinely convincing evidence — an offset that was too aggressive,
actually faulted, and was actually reverted early instead of after
`hold_secs` — will only ever come from an offset search that goes wrong
by accident during ordinary use after this ships, not from a test suite
manufacturing the failure on purpose.

## Deliberately excluded from this plan

- **ECC error counters** (`nvmlDeviceGetTotalEccErrors`,
  `nvmlDeviceGetMemoryErrorCounter`) — present on this GPU's symbol table
  but the bitmask shows single/double-bit ECC events as *unsupported*
  here (`0xf19c` lacks bits `0x1`/`0x2`), and a consumer GPU without ECC
  memory has nothing meaningful to count. Worth revisiting only on
  hardware that actually has ECC.
- **Clock/thermal throttle reasons** (`nvmlDeviceGetCurrentClocksEventReasons`)
  as a revert trigger — a card throttling under an overclock is often
  *working as intended* (thermal or power throttling is the safe
  response, not a fault), so wiring it into the same "revert now" path
  as a critical fault would revert overclocks that were never actually
  dangerous. This is a metric worth exposing in `status()` some day, not
  a trigger for this watchdog.
- **A general NVML event feed** for the app to display live — this plan
  is scoped to the one thing that protects the user (revert on a fault
  during the confirm window), not a monitoring feature.
