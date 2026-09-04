# IPC protocol (app ⟷ daemon)

The Tauri app and `pyren-daemon` are separate OS processes (see
`00-design-plan.md` for why). They talk over a Unix domain socket using a
small JSON-RPC-like protocol. This document is the source of truth for that
wire format — keep it in sync with `daemon/crates/core/src/lib.rs` (server)
and `app/src-tauri/src/lib.rs` (client) if either changes.

## Transport

- Unix domain socket, `SOCK_STREAM`.
- Path: `$PYREN_SOCKET`, falling back to `/tmp/pyren-daemon.sock` for
  unprivileged local development. Production (daemon running as a root
  systemd service) sets `PYREN_SOCKET=/run/pyren/daemon.sock`.
- **Access control is the socket's file mode, and nothing else.** There is
  no authentication in the protocol: opening the socket *is* the
  authorization, so the daemon binds it `0660`, owned by the group
  `pyren` (override with `PYREN_SOCKET_GROUP`). Members of that group
  and root can connect; nobody else can. Where the daemon creates the
  runtime directory itself it is `0750` and owned by the same group; under
  systemd the directory is left world-traversable and the socket inside it
  is the gate.
  - If the group does not exist, the socket stays `0600` — the daemon says
    so at startup, and the app turns the resulting `EACCES` into "add this
    user to the 'pyren' group" rather than a bare I/O error.
  - There is deliberately **no read-only tier** for non-members. Serving
    `getStatus`-style methods to any local process would mean opening a
    root daemon's socket to everything on the machine, sandboxed and
    compromised processes included, to save an admin one `usermod -aG`.
  - Consequently every method in this document assumes an already
    authorized caller. Do not add a method whose safety depends on *which*
    group member called it; the protocol cannot tell them apart.
- Framing: one JSON object per line (`\n`-terminated). No length prefix,
  no batching — a client sends one request line and reads one response
  line before sending the next on that connection. The server accepts
  multiple requests per connection (handled sequentially) and spawns one
  thread per connection.

## Request

```json
{ "id": 1, "module": "fan", "method": "getStatus", "params": null }
```

- `id`: caller-assigned, echoed back unchanged. Not currently used for
  out-of-order/concurrent request matching (each connection is
  request-then-response, one at a time) — reserved for when that changes.
- `module`: target module's `id()` (e.g. `"fan"`), or the built-in `"core"`
  namespace (currently only `core.capabilities`).
- `method`: module-specific method name.
- `params`: arbitrary JSON, module- and method-specific. Omit or pass
  `null` for no-argument methods.

## Response

Exactly one of `result` / `error` is present:

```json
{ "id": 1, "result": { "...": "..." } }
{ "id": 1, "error": { "kind": "permissionDenied", "message": "writing /sys/…/pwm1_enable needs root: Permission denied (os error 13)" } }
{ "id": 1, "error": { "kind": "notCapable", "message": "Clock offsets can be set. This card's clocks cannot be pinned.", "key": "overclock.gpu.nvidia.settable.noLock", "params": {} } }
```

**Branch on `kind`, show `message`.** The message is written for a person
and gets reworded; anything matching on the prose breaks the next time
somebody improves a sentence. `kind` is a closed set:

| `kind` | what it means | what a caller should do |
|---|---|---|
| `unknownModule` | no module with that id is loaded | fix the call |
| `unknownMethod` | the module is there, the method is not | fix the call |
| `unsupported` | this module's hardware is absent on this machine | hide the feature |
| `notCapable` | the hardware is present and cannot do this *particular* thing | hide **this control**, not the feature |
| `invalidParams` | the caller sent something wrong | fix the call; no privilege makes it work |
| `permissionDenied` | the daemon is running unprivileged | offer to restart it as root |
| `io` | the machine refused while the work was being done | show it |
| `busy` | something else has the hardware | wait and ask again |
| `internal` | the daemon could not serialise its own reply | a bug here |
| `failed` | a genuine runtime failure that is none of the above | show it |
| `malformedRequest` | what arrived was not a request; carries `id: 0` | a bug in the client |

Three of these are the reason the field exists at all, because `fan.setMode`
returns all three and they used to be indistinguishable prose:

- `notCapable` — board 8D2F has no `pwm1`, so `manual` will never work
  there however it is asked. **Do not offer to elevate.**
- `permissionDenied` — the same call works fine against a root daemon.
- `invalidParams` — `pwm` was 300, and neither privilege nor different
  hardware helps.

Conflating the first two is the specific mistake this prevents: a UI that
offers "run as administrator" for hardware that will never comply, or that
reports working hardware as unsupported because the daemon lacked root.

**An unknown `kind` is not an error.** A newer daemon may name one this
client has never heard of; treat it as `failed` and show the message.
Refusing to parse a refusal is worse than showing it. For the same reason a
client should still accept a bare-string `error` — that is a daemon from
before this format — rather than reading an unparseable error as *absent*,
which would turn a refusal into a silent success.

### Translatable messages (`Msg`)

Any user-facing sentence — an `error` above, or a prose field in a result
(`detail`, `note`, …) — may arrive as a **`Msg`** instead of a bare string:

```json
{ "key": "overclock.gpu.nvidia.readable.noLock", "params": { "mhz": 3090 }, "text": "Clock offsets are readable. This card's clocks cannot be pinned." }
```

- `text` — the English wording, already interpolated. **Authoritative for
  any consumer that does not localise** (the CLI, `--json` bug reports, the
  log). Always present.
- `key` — a stable catalog key. A client that ships a translation catalog
  (the desktop app, `app/src/lib/i18n/locales/*.json`) renders `key` +
  `params` in the user's language; a client without the key shows `text`.
  Empty string (or absent) means "prose only, show `text`".
- `params` — `{name}` values for the translated string; omitted when empty.
  A joined message carries `params.parts`, a list of `Msg` to translate and
  re-join.

On an `error`, `key`/`params` sit beside `kind`/`message` (the `message` is
the same string as `text`). Raw operating-system error text — the tail of a
failed `exec`, an `io::Error` — is passed through inside a `param` and is
**not** translated.

A client that predates this shows `message`/`text` and loses nothing.

**Through the desktop app's Tauri shell.** A Tauri command's error is a
`String`, so `daemon_error` in `app/src-tauri` forwards a keyed refusal as
its whole `{ kind, message, key, params }` object serialised to JSON; the
frontend's `call()` parses that back into a `DaemonRefusal` and
`errorText()` renders it with the catalog. A refusal with no `key`, and a
bare-string transport failure, cross as the plain message as before. The
dev-server bridge (`vite dev`) passes the object through directly.

`pyren_core::client` does all of this already, and is the copy to follow:
`ClientError::Daemon { kind, message }`, with `needs_root()` for the
`permissionDenied` case. `pyren-ctl` turns the kind into an exit code — `2`
fix the command, `3` cannot reach the daemon, `4` this machine will not do
it, `5` the daemon needs root, `6` busy — so a shell script can branch
without reading English.

## Built-in `core` module

| method | params | result |
|---|---|---|
| `core.capabilities` | none | `[{ "id": string, "supported": bool }, ...]` — every registered module and whether its hardware was detected. Use this to decide which UI sections to show. |
| `core.nextEvent` | `{ "since"?: number, "timeoutMs"?: number }` | `{ "seq", "events": [...], "missed" }` — see below. Does not answer until something happens or the timeout runs out. |

Note that `system` always reports `supported: true` (any Linux machine can
report its own vitals). Whether *OMEN hardware control* is possible is a
different question, answered by `system.getInfo`'s `compatibility` field.

### Events, and the one long poll that carries them

Everything else in this protocol is a question a client asks. The hotkey is
not: the user presses it, and the widget that draws the result has to hear
about it in the tens of milliseconds a person notices — not on the next
poll of `power.getState`.

**The framing does not change.** A client still sends one request line and
reads one response line; `core.nextEvent` simply does not answer until
there is something to say. Nothing in §Transport is revisited, no existing
client is affected, and a client that never calls it never notices.

```json
{ "id": 4, "module": "core", "method": "nextEvent", "params": { "since": 12, "timeoutMs": 25000 } }
{ "id": 4, "result": { "seq": 14, "missed": 0, "events": [
  { "seq": 13, "topic": "hotkey.pressed", "ageMs": 4,
    "payload": { "action": "powerCycle", "mode": "balanced", "from": "eco",
                 "askedFor": "balanced", "changed": true,
                 "applied": ["power-profiles-daemon: balanced"], "failed": [] } },
  { "seq": 14, "topic": "power.mode", "ageMs": 4,
    "payload": { "mode": "balanced", "source": "hotkey" } }
] } }
```

- `since` is the `seq` from the previous reply. **Omit it on the first
  call**, which means "start from now": a client that has just connected
  almost never wants the key presses of a minute ago, and the difference is
  an OSD that stays quiet at login rather than flashing on startup.
- `timeoutMs` defaults to 25 s and is capped at 60 s, so a client cannot
  pin a daemon thread indefinitely. A poll that times out is a normal
  reply with an empty `events` — not an error.
- `seq` comes back whether or not anything did, so a client that polled
  with a stale `since` catches up.
- **`missed` is the honest half of a bounded buffer.** The daemon keeps 64
  events; a client that fell further behind than that is told how many it
  lost rather than handed a gap it cannot see. Non-zero means "re-read the
  state", not "replay".
- A daemon that restarted counts from zero again, so a `seq` *lower* than
  the one held is a new daemon, not an error — adopt it.
- **An unknown `topic` is not an error**, for the same reason an unknown
  `error.kind` is not: a newer daemon may publish one this client has never
  heard of, and ignoring it is the correct response.

Topics so far:

| topic | published when | payload |
|---|---|---|
| `hotkey.pressed` | the bound key was pressed (or `hotkey.press` was called) | `{ action: "show", device, mode }` — the mode in force, so the widget can draw it |
| `power.mode` | the power mode actually moved, **whoever moved it** | `{ mode, source }` |

`power.mode` is published for *every* change that took effect, not only the
ones this daemon was asked for by a key. `source` says who asked:

| `source` | who |
|---|---|
| `request` | a `power.setMode` — the app, `pyren-ctl`, the widget's own click |
| `hotkey` | the laptop's performance key |
| `auto` | the daemon's supervisor, on battery or under load |
| `tuning` | a `power.setTuning` that re-applied the mode in force |
| `osProfile` | a `power.setApplyToOsProfile` that re-applied it |

A change that was *refused* publishes nothing here: the mode did not move,
and a UI that redrew for it would be showing a mode the machine is not in.

### The key shows; it does not change

`hotkey.pressed` carries `action`, and a current daemon always sends
`"show"`: the press puts the widget on screen with the mode in force
highlighted and **changes nothing**. The user picks by clicking a mode in
the widget, which is an ordinary `power.setMode` like any other.

Pressing it again puts the widget away — the widget's own doing, not the
daemon's, which publishes the same event either way. One key, both
directions: the way to dismiss something you opened is the key you opened
it with, rather than waiting out a timer. That only works because the
daemon coalesces repeats first (`repeatGuardMs`): a bare vendor key sends
the same scancode on press *and* release, and untreated that would open
and close the widget on one physical press, leaving the key looking dead.

This is a deliberate departure from the vendor's Fn+P, which steps to the
next profile. The reason is that on this hardware the vendor key never
reaches Linux at all, so the shortcut is one the *user chose* — and a
chosen shortcut that moves the machine to the next profile every time it
is pressed is a worse deal than one that opens a picker.

Daemons before this sent `action: "powerCycle"` with the outcome of the
step — `mode`, `from`, `askedFor`, `changed`, `applied`, `failed`. A
client that wants to work against both should branch on `action`, treat
an absent one as `powerCycle`, and read `changed: false` with `failed` as
"the key worked, the machine refused". `power.cycle` still exists in the
power module for whoever wants that behaviour back behind a setting.

**A client is told even about its own changes.** Filtering those out would
need the daemon to know which connection is which, and would be wrong the
first time two windows were open. One extra `getState` is cheaper than
that. Clients that must always be right ask `power.getState` when they
reconnect anyway, since the machine can move while nobody is listening.

## `system` module

Generic Linux monitoring and machine identification. Unlike `fan`, this
module is **not HP-specific** — it reads `/proc`, `/sys` and `statvfs`, so
it reports real data on any machine. That is deliberate: it lets the vitals
UI be developed and tested away from an OMEN laptop, and it answers the
"is this hardware compatible?" question the rest of the app depends on.

| method | params | result | status |
|---|---|---|---|
| `system.getInfo` | none | machine identity (see below) | ✅ implemented, cached at daemon startup |
| `system.getMetrics` | none | live readings (see below) | ✅ implemented, no privileges needed |

### `system.getInfo`

```json
{
  "vendor": "ASUS", "model": "PRIME B660M-K D4",
  "boardName": "PRIME B660M-K D4", "boardVendor": "ASUSTeK COMPUTER INC.",
  "biosVersion": "3010", "biosDate": "12/11/2023",
  "kernel": "7.2.2-1-cachyos",
  "cpu": "12th Gen Intel(R) Core(TM) i5-12400F", "cpuCores": 12,
  "gpus": ["NVIDIA Corporation GA106 [GeForce RTX 3060]"],
  "formFactor": "desktop",
  "compatibility": "controllable",
  "controls": { "fanMode": false, "fanSpeed": false, "powerMode": true },
  "supported": true,
  "reason": { "key": "system.reason.accepts", "params": { "list": "power modes" }, "text": "this machine accepts: power modes" },
  "privileges": { "root": true, "perfEvents": true }
}
```

`reason` is a **`Msg` object** (see *Translatable messages* above). The
`tools/pyren-check.sh` reference script emits it as the plain English
string; the wording is what the parity test holds to, not the shape.

`privileges` describes the **daemon**, not the machine: what it was started
with, fixed at startup. `perfEvents` is whether the i915 perf PMU opened,
which is what decides whether integrated-GPU utilisation can be read at all
(it needs `CAP_PERFMON`). Without this block an app cannot tell "this GPU
reports nothing" from "we were not allowed to ask", and ends up telling the
user their hardware is broken when the answer is "run me as root".

Identity fields are read from `/sys/class/dmi/id`, `/proc/cpuinfo` and
`lspci`. Firmware placeholder strings (`"To be filled by O.E.M."`,
`"System Product Name"`) are reported as `null` rather than shown to the
user verbatim.

#### `controls` and `compatibility` are measured, not looked up

`controls` is what each hardware module reported it could **actually do**
on this machine, collected by the daemon at startup:

| field | true when |
|---|---|
| `fanMode` | `pwm1_enable` exists, so auto and max can be commanded |
| `fanSpeed` | `pwm1` exists, so a specific speed can be commanded |
| `powerMode` | some power mechanism answered — ACPI platform profile, power-profiles-daemon, or the CPU's energy-performance hint |
| `lightbar` | the 4-zone light strip answered an ACPI read. Named for what was probed rather than "lighting": the per-key keyboard is a different device on a different bus, and a machine can have either, both or neither |

`compatibility` is only their summary:

| value | meaning |
|---|---|
| `controllable` | at least one of `controls` is true; `reason` says which |
| `monitoringOnly` | the `hp-wmi` interface is present and nothing accepted control — fan speeds and temperatures still read fine |
| `unsupported` | neither; this is an ordinary Linux machine as far as this app is concerned |

**Gate UI on `controls`, not on `compatibility`.** A machine can be
`controllable` and still refuse the thing a given page wants: board `8D2F`
has `fanMode` without `fanSpeed`, so the fan page must offer auto and max
and hide the percentage.

This replaced a hand-copied list of DMI board ids, and the reason is worth
keeping: the list was wrong in both directions. It called `8D2F`
"supported" on a machine that cannot set a fan speed, and it would have
called an unlisted board that works perfectly "untested" — while needing to
be extended by hand, one board at a time, for a driver this project does
not install and cannot vouch for. What a machine accepts is a question the
machine can be asked.

### `system.getMetrics`

```json
{
  "cpu": { "usagePercent": 47.8, "perCorePercent": [...], "clocksMhz": [...], "tempC": 56.0 },
  "memory": { "totalGb": 15.4, "usedGb": 6.9, "availableGb": 8.5, "percent": 45.0,
              "swapTotalGb": 33.4, "swapUsedGb": 3.5 },
  "temperatures": [{ "chip": "coretemp", "label": "Package id 0", "celsius": 56.0 }],
  "fans": [{ "chip": "nct6798", "label": "fan1", "rpm": 1012 }],
  "disks": [{ "mount": "/", "device": "/dev/nvme0n1p3", "fstype": "btrfs",
              "totalBytes": 999129350144, "freeBytes": 484879581184 }],
  "network": { "upMbps": 0.01, "downMbps": 0.0, "interfaces": [...] },
  "gpus": [{ "name": "NVIDIA GeForce RTX 3060", "driver": "nvidia", "usagePercent": 99.0,
             "tempC": 74.0, "memUsedMb": 7161.0, "memTotalMb": 12288.0,
             "powerW": 169.5, "clockMhz": 1837.0, "integrated": false }],
  "processes": [{ "pid": 32556, "name": "re4.exe", "cpuPercent": 41.8,
                  "memMb": 2847.0, "gpuPercent": 62.4 }]
}
```

Notes on the shape:

- **Rates are deltas** (CPU %, network throughput, per-process CPU), so the
  daemon keeps the previous sample. The sampler is primed at construction,
  so the *first* call already returns real numbers rather than zeroes.
- `cpu.tempC` prefers a package sensor from a CPU driver (`coretemp`,
  `k10temp`, `zenpower`), then the hottest core, then `acpitz`.
- `disks` deduplicates by device, so btrfs subvolumes and bind mounts show
  once (at the shallowest mount point) instead of eight times.
- `processes` is the busiest 12 **by CPU**; `cpuPercent` is a share of the
  **whole machine** (0–100), not of one core.
- `processes[].gpuPercent` comes from the kernel's DRM fdinfo interface
  (`/proc/<pid>/fdinfo/<fd>`, the `drm-engine-*` keys) — the same source
  `nvtop` reads, covering i915, xe and amdgpu. Two subtleties it handles:
  several file descriptors can name the **same** `drm-client-id`, each
  reporting that client's whole counter (a compositor holding four of them
  is not four times as busy), and an engine can have several instances
  (`drm-engine-capacity-video: 2`), over which its nanoseconds are spread.
  `null` means the driver publishes no per-client accounting — NVIDIA's
  proprietary driver does not — which is not the same as an idle process,
  so the UI draws the two differently.
- `gpus` uses `nvidia-smi` for NVIDIA cards and DRM sysfs for others:
  `gpu_busy_percent` and `mem_info_vram_*` (amdgpu), `gt_act_freq_mhz` or
  `tile0/gt0/freq0/act_freq` for the Intel GT clock, and the i915 perf PMU
  for Intel utilisation, which needs `CAP_PERFMON` (see `getInfo`'s
  `privileges`). Names are resolved once through `lspci`. Fields a driver
  doesn't expose are `null`.
- `gpus[].integrated` distinguishes the chip in the CPU package from a card
  of its own, decided by the PCI slot: integrated GPUs hang off the root bus
  (`0000:00:…`), a discrete card sits behind a PCIe bridge. Hybrid laptops
  have both and the UI labels them separately, because which one is busy is
  usually the question being asked.
- Virtual network interfaces (`lo`, `veth*`, `docker*`, `br-*`, `virbr*`,
  `vnet*`, `tap*`) are excluded so container traffic isn't counted twice.

## `power` module

The Eco / Balanced / Performance / Unlimited switch, plus the background
supervisor that can drive it automatically.

| method | params | result | status |
|---|---|---|---|
| `power.getState` | none | current mode, backend state, power envelope, battery, supervisor config | ✅ implemented |
| `power.setMode` | `{ "mode": "eco" \| "balanced" \| "performance" \| "unlimited" }` | `{ "applied": [...], "failed": [...] }` | ✅ implemented |
| `power.setAutoConfig` | full auto config object | stored config + whether it reached disk | ✅ implemented |
| `power.setRestoreOnStart` | `{ "enabled": bool }` | as above | ✅ implemented |
| `power.setTuning` | `{ "mode"?, "pl1W"?, "pl2W"?, "turbo"? }` | as `getState` | ✅ implemented |
| `power.setApplyToOsProfile` | `{ "enabled": bool }` | as `getState` | ✅ implemented |

### A mode is a profile, in three separable parts

| part | mechanism | applied |
|---|---|---|
| the laptop's own profile | ACPI `platform_profile` | always |
| the OS profile | power-profiles-daemon | only when `applyToOsProfile` |
| the power envelope | powercap PL1/PL2 + turbo | only where someone set it |

**These are three different owners, which is why they are three switches.**

The *laptop's* profile is the firmware's, and it is the one this project
cannot replicate: changing it changes the EC's own temperature-to-RPM
curve, so Eco makes the fans start **later** rather than merely spin
slower, and it moves internal power states — PCIe link power and friends —
that no userspace knob reaches. When a machine has one, it is the most
valuable third of a mode.

The *OS* profile is what the desktop's battery menu shows, and it is
optional on purpose: changing how the laptop behaves without changing what
the desktop thinks is a reasonable thing to want. It is also **delegated,
not reimplemented** — power-profiles-daemon already drives EPP and the
governor for the running system, and writing those files ourselves on top
of it would be two things fighting over them. The per-CPU
energy-performance hint is only used as a fallback, on a machine with no
power-profiles-daemon at all.

The *envelope* is the half the fans feel where there is no firmware profile
to lean on — watts the package may not draw are heat that does not have to
be moved — and **it ships untouched on purpose**.

| | Eco | Balanced | Performance | Unlimited |
|---|---|---|---|---|
| firmware profile | `low-power` | `balanced` | `performance` | `performance` |
| OS profile | `power-saver` | `balanced` | `performance` | `performance` |
| PL1 / PL2 | *stock* | *stock* | *stock* | *stock* |
| turbo | on | on | on | on |

Every laptop has its own internal profiles, and their curves are not each
other's. A percentage that is a sensible Eco on one chassis is a
thermally-throttled mess on the next, and the daemon has no way to tell
which it is looking at. So it ships no opinion: a mode drives the
mechanisms the machine itself provides, and the envelope stays where the
firmware set it until someone puts a measured number in through
`power.setTuning`. That is also why the numbers are stored as a percentage
of **this machine's own stock limits**, captured before the daemon ever
writes one — a value measured on a 77 W laptop means something different on
a 15 W one, and a percentage at least travels honestly.

`getState` reports the whole picture:

```json
"limits": {
  "available": true, "turboAvailable": true,
  "stock":   { "pl1Uw": 77000000, "pl2Uw": 77000000, "pl4Uw": 168000000 },
  "current": { "pl1Uw": 77000000, "pl2Uw": 77000000, "pl4Uw": 168000000 },
  "turbo": true,
  "tuning": { "eco": { "pl1Percent": 100, "pl2Percent": 100, "turbo": true }, "...": {} }
}
```

Four rules worth knowing before writing a client:

- **Nothing ever asks for more than stock.** Raising a limit past what the
  firmware shipped is overclocking, and is a separate feature with separate
  consent — not something a mode does on the user's behalf.
- **`setTuning` speaks watts, the daemon stores percentages.** Watts are
  what the user is shown; percentages are what survives being restored onto
  different hardware.
- **PL4 is left at stock.** The peak-power ceiling exists to keep the VRM
  in spec, and lowering it buys nothing a lower PL1 has not already bought.
- **Applying a mode never touches the fans.** A lower limit makes them spin
  less because there is less heat. Reaching into the fan module to also
  command a fan mode would put two owners on one piece of hardware.

`setTuning` defaults to the mode currently in force and re-applies it
immediately when that is the one changed, so a slider is audible now rather
than at the next mode switch.

### Mechanisms

The OS half of `setMode` tries, in order of how directly it maps to what
the OMEN Gaming Hub does:

1. **`/sys/firmware/acpi/platform_profile`** — the firmware-level switch
   behind Fn+P on HP laptops. Mode names are matched against
   `platform_profile_choices`, since firmware exposes only a subset of the
   ACPI vocabulary (`low-power`/`quiet`/`cool` all count as Eco).
2. **power-profiles-daemon** (`powerprofilesctl`), used when the firmware
   exposes no profile of its own.
3. **`energy_performance_preference`** (intel_pstate/amd_pstate EPP),
   applied on every CPU alongside either of the above.

Each is best-effort, so the result **lists what actually changed** rather
than reporting a success it can't verify:

```json
{ "applied": ["power-profiles-daemon=performance"],
  "failed": ["energy_performance_preference: Permission denied (os error 13)"] }
```

If nothing at all could be applied the call returns an error instead.
Writing EPP and `platform_profile` needs root; power-profiles-daemon
accepts an unprivileged caller through polkit, which is why a daemon run
with `cargo run` can still change that one.

`Unlimited` maps onto the same firmware profile as `Performance` — what
makes it different is the manual fan and power limits applied on top, not a
different platform profile.

### Automatic switching

`power.getState`'s `auto` object configures a supervisor thread that runs
whether or not the app is open:

It is **two systems**, matching the two switches on the app's home screen,
plus a thermal rule that applies to both:

| | when it acts | what it does |
|---|---|---|
| `ecoOnBattery` | the machine is unplugged | drops to Balanced *at once*, then to Eco if it stays idle or the battery gets low |
| `performanceOnLoad` | the machine is plugged in | steps up to Performance *at once*, then back to Balanced if it sits idle |
| `backOffWhenHot` | the machine is over `tempHighC` | holds it at the quiet end of whichever range applies until it is back under `tempLowC` |

So a change of power source is a discrete event with an immediate answer,
and everything after it is a slow refinement inside the range that source
allows:

```text
  on battery:   Eco  <--->  Balanced
  on mains:            Balanced  <--->  Performance
```

Three consequences worth knowing:

- **Unlimited is never chosen automatically.** It is the one mode that
  removes the daemon's own limits, so it is the one mode the user has to
  ask for. The supervisor does move *out* of it when the power source
  changes — unplugging is a deliberate act, and a laptop running unlimited
  off a battery is not what anyone meant — but it never refines its way in.
- **No amount of load reaches Performance on battery**, and no amount of
  idling reaches Eco on mains. The ranges do not overlap.
- **A manual `setMode` suspends refinement, not transitions.** Plugging the
  machine in is the user speaking too, and more recently than the last
  click.
- **Heat outranks load.** A machine is hot *because* it is busy, so the two
  arguments always arrive together; if load won, the thermal rule would
  never fire at all. It does not outrank the user, though — like the other
  rules it leaves `Unlimited` alone and it is suspended by a manual change.

| field | meaning |
|---|---|
| `enabled` | master switch |
| `loadHigh` / `loadLow` | 1-minute load average **per core** above/below which load counts as high/low. The gap between them is a dead band where the supervisor has no opinion — this is what stops the mode flapping around a threshold. |
| `batteryLowPercent` | at or below this charge, Eco is preferred on battery whatever the load is doing |
| `samplesToSwitch` | consecutive agreeing samples required before a *refinement*. Transitions ignore it. |
| `intervalSecs` | how often it samples |
| `manualOverrideSecs` | how long a manual `setMode` suspends refinement — whoever is at the keyboard wins |
| `backOffWhenHot` | whether a hot machine is a reason to step down. On by default; the case it exists for is a laptop on a duvet |
| `tempHighC` / `tempLowC` | the temperature at or above which the machine counts as hot, and the one it has to come back below before it stops counting. **Latched between the two**, and the band is wider than the load one on purpose: a chassis that has just been throttled is still full of heat, and a single threshold would step straight back into the same wall |

The load average is used rather than instantaneous CPU usage precisely
because it is already smoothed: a mode switch spins fans up or down and is
very visible, so only *sustained* load should trigger one.

The temperature is the hotter of the CPU package and the GPU, read from
hwmon by driver name (`coretemp`/`k10temp`, `amdgpu`/`nouveau`/`nvidia`/
`radeon`) rather than from whichever `temp1_input` turns up first — most of
what is in `/sys/class/hwmon` on a laptop is the temperature of something
else. A part reading 0 is powered down rather than cold, so it is left out
of the comparison; a machine with no sensor at all never becomes hot, and
never stops being hot either if it somehow got there, because losing a
sensor is not evidence of cooling.

`getState` reports what the rule can see, as `thermal`:

```json
"thermal": { "available": true, "tempC": 74.0, "hot": true }
```

`hot` is latched, so it is deliberately not something a client could
recompute from `tempC` — at 74 C it can be either, depending on which
threshold was crossed last.

`getState` also reports `autoOverrideSecondsLeft` and `lastAutoSwitch` so
the UI can explain why the supervisor is or isn't acting. `lastAutoSwitch`
is a **`Msg` object** (the reason the mode last moved — "battery at 15%",
"plugged in"), and a `setMode` / `setTuning` refusal carries `key`/`params`
beside its `message`.

### Persistence

Settings are stored in `power.json` (see "Config" in `00-design-plan.md`),
so the supervisor keeps running with the user's rules after a reboot -
which is the point of it being a daemon rather than part of the app.
`getState` reports `configPath`, and `configSaveError` when the last write
failed.

The calls that change settings return `{ saved, saveError }` alongside the
config. A setting that was applied but not written is not an error - it is
in effect right now, it just won't survive a restart - so the call
succeeds and the UI says so rather than failing.

`restoreModeOnStart` re-applies the saved mode when the daemon starts. It
is **off by default**: changing a machine's power behaviour at boot should
be something the user asked for. Enabling it records the current mode
immediately, so a reboot restores what the user could see when they ticked
the box.

### Battery detection

`supply.onBattery` is `null` on machines with no battery, which is **not**
the same as "on battery" and must not be treated as such. Peripherals
(wireless mice, keyboards, headsets) also register under
`/sys/class/power_supply`; they are excluded via `scope=Device`, without
which a discharging mouse makes a desktop look like an unplugged laptop.

## `installer` module

Ports the source project's `install_driver.sh` and the install paths in
`omen_logic.py`.

> **This is not the first thing to reach for.** Manual fan control is
> upstream in recent kernels, so installing a patched out-of-tree driver is
> usually a downgrade — but not always: a stock driver that comes up
> *without* `pwm1` did not recognise the board, and then this is the
> remedy, as it was on the test laptop. Use `fan.diagnose` to verify what
> the running kernel already does; the driver actions here remain for
> the case where a board genuinely isn't supported by the stock driver, and
> `installService`/`removeService` are still the normal way to install the
> daemon's systemd unit.

Split into **inspect → plan → apply** rather than one imperative script: installing means unloading a kernel module, replacing a
file under `/lib/modules` and regenerating the initramfs, so the user
should see exactly what will run before authorising it — and a rendered
plan is also something that pastes into a bug report.

| method | params | result | status |
|---|---|---|---|
| `installer.inspect` | none | what this machine has, and whether the patch is needed | ✅ implemented |
| `installer.autodetect` | none | the install's inputs, worked out from the machine | ✅ implemented, read-only |
| `installer.plan` | `{ action, preferHooks?, force? }` | ordered steps, blockers, warnings | ✅ implemented |
| `installer.apply` | as above plus `confirm`, `auto`, `skipSteps`, `cpuMaxRpm`, `gpuMaxRpm`, `experimentalBoard`, `boardTable` | `{ plan, report, autodetected? }` | ✅ implemented; `installDriver` run for real on 8D2F |

All four are driven from the app by `DriverWizard.svelte` at the bottom
of `/drivers`, which renders the plan's steps and their commands and keeps
"apply" disabled until a dry run of those exact options has come back —
see `docs/03-frontend.md`. `pyren-ctl` has no installer subcommand; the
wizard and `--install-service` are the two ways in.

### `autodetect`: the inputs, instead of a form

An install needs four answers that used to be typed by hand — the two fan
ceilings, the board id, and which of the driver's tables that board belongs
in. Every one of them is already on the machine, so `autodetect` reads them
instead of asking:

| answer | read from |
|---|---|
| board id, model, family | DMI (`/sys/class/dmi/id`) |
| whether the driver already knows the board | `hp_wmi_feature_boards[]` in the driver's own `hp-wmi.c.orig` |
| `cpuMaxRpm` / `gpuMaxRpm` | `fan1MaxRpm` / `fan2MaxRpm` in `fan.json`, i.e. the last `fan.calibrate` run |

```json
{
  "dmi": { "boardName": "8D2F", "productName": "OMEN Gaming Laptop 16-am0xxx",
           "productFamily": "103C_5335M7 HP OMEN", "sysVendor": "HP" },
  "family": "omen",
  "boardKnown": false,
  "experimentalBoard": "8D2F",
  "boardTable": { "table": "features", "params": "omenV1NoEc" },
  "cpuMaxRpm": null, "gpuMaxRpm": null, "rpmSource": "driverFallback",
  "notes": [ { "key": "installer.auto.boardNew", "params": {"board": "8D2F", "params": "omen_v1_no_ec"}, "text": "…" } ]
}
```

`notes` are `Msg` objects: the reasoning behind each answer, so the wizard
can show *why* rather than presenting a filled-in form as fact.

Two things it will not do. It never claims a board-params variant for a
machine that names neither family (`family: "unknown"` leaves
`boardTable` null and says so), because the variants write different
thermal-profile values over WMI. And an uncalibrated machine gets
`null` ceilings rather than a guess — the driver asks the firmware for one
and only falls back to a compiled-in number, which is better than a number
Pyren made up.

#### The board-params variant is measured, not guessed

The four variants differ in one thing DMI cannot say: which EC byte holds
the thermal profile. `autodetect` settles it by looking, in three steps:

1. **Does it matter?** All four share one fan profile, and a board already
   in `omen_thermal_profile_boards` or `victus_thermal_profile_boards`
   takes that thermal-profile path and never reads the variant's offset.
   `paramsEffect` reports `inertOmenPath` / `inertVictusPath` for those,
   and saying "this changes nothing here" beats a caveat about a choice
   with no effect.
2. **If it does, read the EC.** With `probeEc: true` the daemon loads
   `ec_sys` (read-only) and reads offsets `0x59` and `0x95`; whichever
   holds a value `enum hp_thermal_profile_omen_v1` uses names the variant.
   `ec` carries what it saw, or why it could not look.
3. **Otherwise say so.** Neither offset holding a profile means the board
   keeps it elsewhere, and `omenV1NoEc` is then right rather than a
   fallback; an unreadable EC gets the same variant but a different note,
   because a guess and a measurement must not read alike.

`probeEc` defaults to **false**: everything else `autodetect` does is a
read of something already present, and loading a kernel module is not
that. The wizard passes `true`, because clicking install is the
authorisation. An `auto` apply probes for the same reason.

`apply` with `"auto": true` runs the same survey and fills in **whatever
the request left unset**; explicit values always win, so a caller can
detect most of it and pin one field. The result carries what was actually
used back as `autodetected`. (The app does not mix the two: its wizard
offers automatic and manual as separate modes, and sends the manual fields
only in manual mode — see `docs/03-frontend.md`.) Which board-params
variant is picked for an unlisted board is a *choice*, not a reading: the
conservative variant of the right family (`omenV1NoEc` / `victusS`, both of
which read no thermal profile back from the EC) is used, and the note says
so.

Each step's `description`, every `blockers[].message`, every `warnings[]`
entry, and a report step's `description` are **`Msg` objects** (see
*Translatable messages* above) — `key` + `text`. A blocker's `fix` and a
step's `command` are shell text, quoted verbatim, not translated. A report
step's `detail` is a `Msg` too: for a planned or skipped step it carries a
`key`; for a real run it is `Msg::literal` wrapping the command's own
output, so it has no `key`.

#### Installing the service cannot go through IPC

Writing the systemd unit is what *makes* the daemon run as root, so asking
an unprivileged daemon to do it over the socket is a chicken and egg the
IPC path cannot break. The binary therefore also takes the action directly:

```sh
sudo pyren-daemon --install-service    # and --remove-service
```

This is not a second implementation — it drives the same
`installer::{plan, execute}` as `installer.apply` with
`action: "installService"`. It is what the app's Permissions panel runs
under `pkexec`.

`action` is one of `installDriver`, `restoreDriver`, `installService`,
`removeService`.

### `skipSteps`: opting out of the optional ones

A step the plan marks `optional` is one whose *failure* it tolerates —
regenerating the initramfs (known to break on odd EFI layouts), unloading a
module that may not be loaded, cleaning a build tree. Those are also the
only steps it can do without, so they are the only ones `skipSteps` accepts:

```json
{ "action": "installDriver", "auto": true, "skipSteps": ["initramfs"] }
```

They come back in the report as `declined`, a status of their own rather
than `skipped` — one is a decision, the other is the wreckage of an earlier
failure, and a report that called them the same thing would hide which
happened.

Naming a **required** step is refused with `invalidParams` (as is naming
one that is not in the plan) rather than ignored. Silently running a step
the caller asked to skip and silently skipping `depmod` are both worse than
an error: the second would leave a module installed that nothing can find
and report success.

### Safety rules

- **`apply` is a dry run unless `confirm: true`.** A mis-sent message can
  never replace a kernel module; the report comes back with every step
  marked `planned`.
- **A plan with blockers is refused**, and the blockers say why, with the
  command that fixes each one where there is one (missing kernel headers
  are the common case, and on Debian they are split across three packages).
- **Installing is refused when fan control already works** (`pwm1` is
  present), unless `force` is set. Manual fan control went upstream in
  Linux 6.20, so on a modern kernel replacing the stock driver is usually a
  downgrade — `inspect` reports `patchNeeded: false` there.
- **A modern kernel with no `pwm1` is warned about as a reason to install,
  not against it.** That combination means the stock driver came up and did
  not claim this board, which is the one case the patch is for; the warning
  (`installer.warn.boardMissing`) says so. Where hp-wmi is not loaded at all
  there is no evidence this is one of these laptops, and that is a
  different sentence (`installer.warn.noHpWmi`). An earlier single warning
  called the patch "probably unnecessary" in both, which contradicted the
  verdict the app shows directly above it.
- **The stock module is always backed up before being removed**, and only
  when no `.bak` exists yet, so re-running an install never overwrites the
  pristine backup with an already-patched module.

### Notes on the port

Two places where the source project's **documentation disagrees with its
own shipped driver**; the code is what gets compiled, so the port follows
that and keeps the documented names only as fallbacks:

- The docs describe one `#define OMEN_MAX_RPM`; `hp-wmi.c` has
  `OMEN_CPU_MAX_RPM` and `OMEN_GPU_MAX_RPM`. Patching by the documented
  name silently does nothing and leaves an uncalibrated fan ceiling.
- The docs describe a `victus_s_thermal_profile_boards` array; no such
  symbol exists. The real table is `hp_wmi_feature_boards`, a
  `dmi_system_id` whose entries also select a board-params variant
  (`victus_s`, `omen_v1`, `omen_v1_legacy`, `omen_v1_no_ec`) — which is why
  `experimentalBoard` also requires `boardTable`. Guessing it would give a
  driver that loads and then reads the wrong EC offsets.

One deliberate deviation from the shell script: it picks
`update-initramfs` first unconditionally, but Arch systems often have a
compatibility shim by that name installed next to the real `mkinitcpio`.
The port picks the generator matching the distribution family first.

### Where the driver sources come from

They ship with Pyren, in `driver/` — a verbatim copy of the upstream tree,
with its provenance in `driver/README.md`. `inspect` looks in
`$PYREN_DRIVER_DIR`, then `/usr/share/pyren/driver` (where a package must
install it), then the repository's own `driver/` (resolved at compile time,
so a development build finds it from any working directory), then a sibling
checkout of the upstream project; it reports a `no-driver-source` blocker
only when all four are missing.

Whichever is found is treated as **read-only**. `apply` copies the tree to
`/usr/src/hp-wmi-omen-1.0/` first and patches *that*, so the snapshot stays
pristine and a second install never starts from the first one's output —
which is why `stage-source` comes before `patch-source` in every plan.

## `fan` module

| method | params | result | status |
|---|---|---|---|
| `fan.getStatus` | none | the status object below | ✅ implemented, read-only, no privileges needed |
| `fan.diagnose` | `{ "allowWrites": bool }` | full self-test report (see below) | ✅ implemented |
| `fan.setMode` | `{ "mode": "auto"\|"max"\|"manual"\|"curve", "pwm"?: 0-255 }` | the status object | ✅ implemented, needs root |
| `fan.setCurve` | `{ "curve": [{ "tempC": number, "percent": number }], "interpolation"?: "smooth"\|"discrete", "referenceSensor"?: "cpu"\|"gpu" }` | the status object | ✅ implemented |
| `fan.setRestoreOnStart` | `{ "enabled": bool }` | the status object | ✅ implemented |
| `fan.calibrate` | `{ "seconds"?: 10-120 }` | the calibration report below | ✅ implemented, needs root, **blocks and spins the fans** |
| `fan.cleanerStatus` | `{ "refresh"?: bool }` | the cleaner status below | ✅ implemented, read-only (`refresh` puts two ACPI *queries*) |
| `fan.startCleaning` | `{ "speed"?: 10-39, "seconds"?: 5-60, "force"?: bool }` | the cleaner status | ✅ implemented, needs root and `acpi_call`, **reverses the fans** |
| `fan.stopCleaning` | none | the cleaner status | ✅ implemented, idempotent |
| `fan.setCleanerConfig` | `{ "seconds"?: 5-60, "speed"?: 10-39 \| null }` | the cleaner status | ✅ implemented |

Every write returns the same status object `getStatus` does, so a caller
never has to follow a write with a read:

```json
{
  "driverInstalled": true,
  "capabilities": { "switchMode": true, "setSpeed": false },
  "cpuTempC": 38,
  "gpuTempC": null,
  "fanRpm": 2097,
  "isReverse": false,
  "mode": "auto",
  "pwm": null,
  "targetPwm": null,
  "manualPwm": 128,
  "curve": [{ "tempC": 40, "percent": 20 }],
  "interpolation": "smooth",
  "referenceSensor": "cpu",
  "referenceSensorInUse": "cpu",
  "gpuSensorAvailable": false,
  "restoreModeOnStart": false,
  "cleaning": false,
  "fanMaxRpm": null,
  "fan1MaxRpm": null,
  "fan2MaxRpm": null,
  "calibrating": false,
  "error": null,
  "saved": true,
  "saveError": null
}
```

### What `capabilities` is for

**Not every machine can do every mode, and the difference is not
cosmetic.** `hp-wmi` exposes two files, and they are independent:

- `pwm1_enable` → `switchMode`: `auto` and `max` can be commanded. These go
  through a WMI query that needs no per-board parameters, so they work on
  boards the driver has no entry for.
- `pwm1` → `setSpeed`: a *specific speed* can be commanded, which is what
  `manual` and `curve` need. The running driver only exposes it for boards
  in its feature table.

Board `8D2F` is the case that makes this real: `switchMode` true,
`setSpeed` false. Asking such a machine for `manual` returns an error
naming what is missing rather than accepting the call and doing nothing, so
**a client should read `capabilities` and hide what it cannot offer**
instead of discovering it from a failed write.

Two further things a client must not assume:

- `pwm = 0` is not "fans off". The driver reads 0 as
  `HP_FAN_SPEED_AUTOMATIC` and hands the fans back to the firmware, so the
  daemon clamps any commanded speed to at least 1.
- `mode` is what is actually in force, which after a daemon restart is what
  the *hardware* was found in, not what the config file says — unless
  `restoreModeOnStart` is on. Until someone asks for a mode, the daemon
  watches and does not write.

### `fan.calibrate`

Measures what "full speed" actually is on this machine: put the fans at
**max**, watch them, keep the peak, put back the mode that was there.

`fanMaxRpm` is the one input the curve's hysteresis wants and otherwise
never has — with it, "is the fan already going roughly this fast" is a
question for the tachometer; without it the daemon compares the PWM values
it last wrote, which assumes a linear PWM→RPM relationship that no fan has.

**It needs only `switchMode`**, since `max` is a mode and not a speed. So
it runs on a board like `8D2F`, which cannot be given a percentage at all —
and there it is the only way to learn the number the driver's own
`OMEN_CPU_MAX_RPM` fallback is standing in for.

```json
{
  "verdict": "measured" | "noReading" | "didNotRespond" | "reverse",
  "fanMaxRpm": 3915,
  "fan1MaxRpm": 3915,
  "fan2MaxRpm": 3745,
  "baselineRpm": 2093,
  "startedAtMax": false,
  "seconds": 12,
  "settled": true,
  "restoredMode": "auto",
  "restoreError": null,
  "detail": "3915 rpm, up from 2093 at idle, settled after 12s",
  "samples": [
    { "atSecs": 1, "fan1Rpm": 2400, "fan2Rpm": 2230, "isReverse": false }
  ],
  "status": { "…": "the same status object every other fan write returns" }
}
```

Four rules a client should know about:

- **The call blocks** for up to `seconds` (default 30, clamped to 10-120).
  There is nothing to return until a physical process finishes. The state
  lock is not held while it runs, so `getStatus` still answers — and
  reports `calibrating: true` — from another connection.
- **It ends early when the reading settles.** On the test laptop the fans
  reach ~3900 rpm in six seconds, so a fixed thirty is twenty-four seconds
  of noise that measures nothing. `seconds` is the ceiling, not the target,
  and `settled` says which happened.
- **A run that measured nothing stores nothing**, and does not erase a
  previous run that did. `didNotRespond` is the case this exists for: a
  machine that accepts `max` and ignores it would otherwise record its
  *idle* speed as its ceiling, which is worse than having no calibration —
  the hysteresis would then believe every target above idle was already
  reached. The one case where no rise is expected, the fans already being
  at max, is recognised rather than guessed at (`startedAtMax`).
- **The mode found at the start is always put back**, including when the
  run fails partway or panics. If it cannot be put back — a machine
  observed in `manual` that has no `pwm1` — the fans go to `auto` rather
  than being left at full speed, and `restoreError` says so.

`samples` is the trace, one reading a second, kept because it is the
evidence for the verdict rather than decoration: a `didNotRespond` is much
easier to argue with when the numbers behind it are in the reply.

There is no dry run, unlike `diagnose`'s `allowWrites`: measuring full
speed means reaching it. The method name is the consent.

`pyren-ctl fan calibrate [--seconds N]` is the same thing from a shell.

### The fan cleaner

Dust removal by spinning the fans **backwards**, ported from the original's
`_cleaner.py`. It is the one feature in this project that bypasses the
kernel driver entirely: HP's `SECU` buffer protocol over `/proc/acpi/call`,
the same dialect `rgb` speaks to the lightbar, through the same
cross-module lock (`pyren_core::acpi`).

```json
{
  "supported": true,
  "generation": "modern",
  "capabilities": { "cpu": true, "gpu": true, "fan3": false,
                    "cpuSpeed": 37, "gpuSpeed": 39, "fan3Speed": 0 },
  "answered": true,
  "unreachable": null,
  "acpiCallLoaded": true,
  "acpiCallInstalled": true,
  "detail": { "key": "fan.cleaner.probe.modern", "params": { "fans": "the CPU fan and the GPU fan" }, "text": "…" },
  "running": true,
  "transitioning": false,
  "secondsRemaining": 22,
  "secondsTotal": 30,
  "speed": 37,
  "fansReversed": true,
  "durationSecs": 30,
  "configuredSpeed": null,
  "maxStartTempC": 70,
  "cpuTempC": 41,
  "error": null
}
```

**Reverse spin is cooling switched off, not turned down.** Everything below
follows from that, and a client that reproduces none of the rest should at
least reproduce this: for as long as a cycle runs the machine has no
working fans.

- **The timeout is enforced three times over**, on purpose: a watchdog
  thread armed when the cycle starts, `Cycle::expired` on every status
  read, and the fan control loop's own tick. Any one of them ending a
  cycle is enough, and none of them is trusted to be the one that does.
- **`startCleaning` blocks only for the braking step** — a few seconds
  while the blades are brought to a stop, because reversing a fan that is
  still turning forwards is a mechanical step, not protocol ceremony. It
  returns with the countdown running; the cycle itself is not held open on
  the connection.
- **The clock starts when reverse spin begins**, not when the call
  arrives. Braking is setup, and charging it against a 30-second cycle
  would make a cycle shorter on the machines whose fans take longest to
  stop.
- **A cycle found still running at daemon startup is ended.** This is the
  one place the fan module touches hardware without being asked, and it is
  a deliberate exception to *the daemon does not touch the fans until
  asked* — that rule is about not imposing a remembered setting at boot,
  and reverse spin is not a setting. It runs only when the tachometers
  themselves report reverse (the driver's own bit, not a file this daemon
  wrote) and only when `acpi_call` is *already* loaded.
- **Stopping is a ramp, not a switch**: the reverse speed is stepped down
  by 5 with the direction bit still set, and only then is the override
  released. `stopCleaning` is idempotent — stopping when nothing runs is
  the state the caller asked for, not an error, because it is the button
  somebody reaches for when they are not sure what is happening.

Two guards refuse a start, and both are `failed` rather than
`invalidParams`: the caller asked for something reasonable and the machine
is in no state for it *now*. Above `maxStartTempC` (70 °C) a cycle would
remove the cooling a machine is currently using; a second cycle while one
is in flight is `busy`.

#### `transitioning` is not `running`

`running` is a cycle with a countdown. `transitioning` is the braking at
the start or the ramp at the end — the fans are the cleaner's, and they
are reversed or about to be, but there is no number to show. **A client
that reads `transitioning` as idle offers a second cycle in the middle of
the first**, and the control loop takes the fans back mid-ramp. Both
states mean *the fans are not yours*.

#### Three reasons to be unavailable, and only one is the hardware

The same distinction the lightbar probe makes, and for the same reason:
**failing to ask is not being told no.**

- `supported: false`, `answered: true` — the firmware was asked and this
  machine has no fan cleaner. Most models do not. A fact, no remedy.
- `supported: false`, `unreachable` set — the question could not be put:
  no `acpi_call`, or a daemon without root. `unreachable` carries the
  sentence saying which, and both have fixes.
- `acpiCallInstalled` without `acpiCallLoaded` is the state between
  installing the package and the next `modprobe`, told apart because the
  remedy differs.

A refusal to *start* follows the same rule, which is why a missing
`acpi_call` is `failed` (it names a package) and never `notCapable` (which
would tell someone their laptop cannot do this).

`force: true` skips only the `notCapable` refusal — not the temperature
guard. It exists because none of the capability decoding has been
confirmed against real firmware (see below), so a machine that has the
feature and answers a query this build reads wrongly would otherwise have
no way to try it.

#### Two firmware generations

`modern` ("CleanCreek") is a 128-byte buffer with a per-fan speed in
hundreds of RPM and bit 7 meaning reverse; speeds are commanded, so the
cycle can be braked, engaged and ramped. `legacy` is a 4-byte control
buffer with one toggle bit — on or off, no speed and no ramp. Which one a
machine has is probed, never guessed from the model.

`speed`/`configuredSpeed` are in hundreds of RPM (37 → ~3700 rpm), which is
the unit the firmware itself uses. `null` means *use whatever the firmware
has configured for itself*, which is what the vendor's tool would send —
distinct from "unset", so `setCleanerConfig` treats an explicit `null` as a
value and an absent field as "leave it alone".

#### Nothing here has been confirmed against hardware

The `(command, command_type)` pairs are reverse-engineered upstream and
**untested against real firmware by this project** — the development
laptop has no `acpi_call` (`dev/FINDINGS.md`). What can be tested without
it is: the buffers built, the replies parsed, the capability decoding, the
guards, and that command type 44 (ask) never becomes 46 (set). Those are
tested, in `cleaner.rs` and in the parity test, so the only untested thing
left is the firmware's own answer.

`pyren-ctl fan cleaner`, `fan clean` and `fan clean-stop` are the same
three calls from a shell.

### `fan.diagnose`

The fan-control self-test. This is the project's answer to "is the driver
right?", and it replaced installing one: manual fan control is upstream in
recent kernels, so on most machines the right answer is *the stock driver
already does this*, and the useful thing is to prove it rather than replace
it.

```json
{
  "verdict": "fullControl" | "monitoringOnly" | "unsupported",
  "summary": { "key": "diagnostics.summary.monitoringOnly", "text": "…" },
  "driverNotice": { "key": "diagnostics.driverNotice.noPwm", "params": { "hint": "…" }, "text": "…" } | null,
  "wroteToHardware": false,
  "checks": [
    { "id": "pwm1",
      "title":  { "key": "diagnostics.checks.pwm1.title", "text": "PWM channel" },
      "status": "pass",
      "detail": { "key": "diagnostics.checks.pwm1.ok", "params": { "value": 128 }, "text": "pwm1 = 128 (0-255)" },
      "remedy": null }
  ]
}
```

`summary`, `driverNotice` and every check's `title` / `detail` / `remedy`
are **`Msg` objects** (see *Translatable messages* above) — a client that
localises renders the `key`; one that does not shows `text`. `verdict`,
`id` and `status` stay bare enum strings. A `kernel-log` check's `detail`
is the kernel's own words, quoted verbatim, so it carries no `key`.

Checks cover the hp-wmi platform device, the hwmon node, both fan inputs
(decoding the reverse-spin encoding rather than reporting a 15200 "rpm"),
`pwm1`, `pwm1_enable` and what its value means, the ACPI platform profile,
the CPU temperature sensor, `acpi_call` and the fan cleaner. Each carries
a `remedy` when there is something to do about it.

**Every check is read-only unless `allowWrites` is set.** That one check
writes the value that is *already* set - so no fan changes speed - and
restores the previous mode afterwards, including when the readback fails.
It needs root, and reports `skip` rather than failing when run
unprivileged.

`driverNotice` is the "there is a driver that might help" message, and only
appears when it would be useful: an HP machine whose kernel exposes no
`pwm1` is told to try a newer kernel **first** and only then the patched
out-of-tree driver; a machine with no hp-wmi at all is told what that means
instead of being pointed at an HP driver it can't use.

The same report is available without the app, from `pyren-check` - see
`docs/02-development.md`.

### Which sensor the curve follows

`referenceSensor` picks it: `cpu` (the default) or `gpu`. The GPU is worth
having because on a laptop it is usually what heats up first under a game,
and a CPU-only curve spins up after the heat has already spread.

The fallback is **one-directional**, and that is the whole design:

- `gpu` falls back to the CPU whenever the card reports nothing. A GPU at
  0 C is one that is powered down, not a cold one, and stopping the fan
  curve while the machine works would be worse than following the other
  sensor.
- `cpu` never falls back to the GPU. Someone who picked the CPU and
  silently got a curve driven by the other sensor would be looking at a
  machine nobody asked for.

So a client has to be told two things rather than one, and the status
carries both: `referenceSensor` is the setting, `referenceSensorInUse` is
what is actually being read right now (`null` when neither sensor answers),
and they differ exactly while the card is asleep. `gpuSensorAvailable`
says whether there is a GPU sensor to offer at all — a machine with
integrated graphics only should be shown no choice rather than one that
does nothing.

Changing the sensor restarts the smoothing window: it holds ten seconds of
the *other* sensor's readings, and averaging across the change would drive
the fans from a temperature neither part is at.

`cpuTempC` is `null` if no CPU temperature sensor was found (mirrors the
Python original's fallback chain: `coretemp`/`k10temp` hwmon →
`thermal_zone0` → give up); `gpuTempC` is `null` far more often, and both
come from `pyren_core::sensors`, which the power supervisor's thermal rule
reads too. `fanRpm` is `max(fan1, fan2)`, decoded through
the hp-wmi reverse-bit encoding — see
`docs/02-kernel-driver.md` in the `omen-fan-control` project (see
`dev/README.md` for where that checkout is) for why raw values
`>= 12800` get remapped.

## `rgb` module

| method | params | result | status |
|---|---|---|---|
| `rgb.getCapabilities` | none | a **fresh** probe of both hardware paths | ✅ implemented, read-only |
| `rgb.getStatus` | none | the probe (re-taken only if an interface changed) plus what this daemon last set | ✅ implemented, read-only |
| `rgb.setZones` | `{ "zones": [c, c, c, c], "brightness"?: 0-100 }` | the status object | ✅ implemented, needs root, **never run against a light strip** |
| `rgb.setStatic` | `{ "color": c, "brightness"?: 0-100 }` | the status object | ✅ implemented, needs root, ditto |
| `rgb.off` | none | the status object | ✅ implemented, needs root, ditto |
| `rgb.readZones` | none | `{ "zones": [c, c, c, c] }` | ✅ implemented, needs root, ditto |
| `rgb.setRestoreOnStart` | `{ "enabled": bool }` | the status object | ✅ implemented |
| `rgb.setDialect` | `{ "dialect": "auto" \| id }` | the status object | ✅ implemented |

A colour `c` goes **out** as `"#rrggbb"` and is accepted **in** as either
that or `[r, g, b]`, so a script does not have to build a hex string to
say 255,0,0. Components outside 0-255 are clamped rather than refused: a
caller that sent 300 meant "as much red as there is", not "reject my whole
request".

`getStatus.error` (the last write failure) is a **`Msg` object**, and a
refusal from a `set*` call carries `key`/`params` beside its `message` —
including the shared `acpi.*` keys when the cause is a missing `acpi_call`.

`brightness` is a **percentage**, not a 0-255 level — that is what the
protocol takes, and calling it brightness while meaning a level is how a
UI ends up with a slider that does nothing above 40 %.

### There is no single OMEN lighting protocol

Three unrelated ways of talking to these lights exist, they share nothing
but the vendor, and **which one a laptop speaks is not decided by its model
name** — the same rule as §"`controls` and `compatibility` are measured,
not looked up". So all three are implemented, all three are probed, and the
first that answers is used.

| id | how | needs | where it comes from |
|---|---|---|---|
| `kernelZones` | `/sys/devices/platform/hp-wmi/rgb_zones/zone00…03`, one `RRGGBB` each | a kernel that publishes them | the in-tree and out-of-tree `hp-wmi` four-zone support |
| `fourZone` | WMI command `0x20009`, command types 2 (`COLOR_GET`) / 3 (`COLOR_SET`); zones at byte 25 of a 128-byte state buffer | `acpi_call`, root | the 2023 `hp-wmi` four-zone patch and `OmenLinux/omen-rgb-keyboard` (2025), read independently and in agreement |
| `lightbar` | WMI command `0x20009`, command type 11 (`SET_LIGHTBAR_COLORS`); brightness at byte 3, zones at byte 7 | `acpi_call`, root | `omen-rgb-linux`, the port this module started as |

They are tried in that order and the order is not arbitrary: `kernelZones`
cannot send the firmware a command it did not expect, so wherever it exists
it is the right answer.

**Probing is reading.** A dialect is *available* when a **read** through it
answered. Nothing probes by writing: a probe that changes the lights is not
a probe, and on a machine that speaks a different dialect it would be a
write of unknown meaning.

That is exactly why `rgb.setDialect` exists. Auto can only ever pick a
dialect this build can *read*; the person at the keyboard can see whether
the lights actually changed. A pinned dialect is therefore used **whether
or not it probed**, and `getStatus` reports the choice and the resolution
separately:

```json
{ "dialect": "auto", "activeDialect": "fourZone" }
```

`dialect` is what was asked for (`"auto"` or an id); `activeDialect` is
what that resolved to, and `null` when nothing answered.

**A brightness slider means the same thing on all three.** Only `lightbar`
has a brightness field in its payload; the other two have none, and their
reference drivers scale the colours in software instead. So does this
module (`pyren_rgb::scale`), which is why brightness is a percentage
everywhere rather than a control that works on one dialect and silently
does nothing on another.

### Two unrelated *devices*, on top of that

Underneath the three dialects there are still two unrelated pieces of
hardware, and only one of them is driven:

| | Per-key RGB | The four zones |
|---|---|---|
| Transport | USB HID, `hidapi` | ACPI-WMI, or the kernel's sysfs files |
| Device | HP Gaming Keyboard II, `0d62:54bf` | `hp-wmi` (+ the `acpi_call` module for the WMI dialects) |

Both are probed; only the second is driven. On the one OMEN this project
has run on there is no `0d62` device on the bus at all. The full reasoning,
and the three upstream bugs this port fixes rather than carries over, are
in [`04-rgb-porting-review.md`](04-rgb-porting-review.md).

`getCapabilities` answers:

```json
{
  "perKey": {
    "present": false,
    "usbId": "0d62:54bf",
    "ported": false,
    "detail": "no HP Gaming Keyboard II on this machine"
  },
  "lighting": {
    "present": false,
    "hpWmi": true,
    "acpiCall": true,
    "acpiCallInstalled": true,
    "commandAnswers": false,
    "unreachable": null,
    "dialects": [
      { "id": "kernelZones", "transport": "the kernel's rgb_zones files",
        "available": false, "asked": false,
        "detail": "this kernel does not publish rgb_zones for hp-wmi" },
      { "id": "fourZone", "transport": "WMI 0x20009, command types 2/3",
        "available": false, "asked": true,
        "detail": "the firmware refused (it answered: )" },
      { "id": "lightbar", "transport": "WMI 0x20009, command type 11",
        "available": false, "asked": true,
        "detail": "the firmware refused (it answered: )" }
    ],
    "detail": "the firmware was asked in every dialect this build knows and refused each one"
  },
  "supported": false
}
```

Four fields there are easy to conflate and must not be:

- **`present` is a claim about hardware.** It is only ever true when some
  dialect was asked and answered.
- **`asked: false` is not a refusal.** It means the question could not be
  put — no kernel files, no `hp-wmi`, no `/proc/acpi/call`, or a daemon
  that is not root — and a client that shows "your machine has no lighting"
  for that is telling the user something nobody established.
- **`acpiCall` and `acpiCallInstalled` are different problems.** Not
  installed needs a package; installed-but-not-loaded needs a `modprobe`.
- **`commandAnswers`** is whether the firmware's lighting command
  (`0x20009`) answered a plain read *at all*, independent of any dialect.
  `true` with no available dialect is the interesting machine: it has
  lighting and none of the three operations this build knows is the one it
  wants. That is a machine to pin a dialect on by hand, not a machine
  without lights.

`getCapabilities` re-probes on every call, unlike `is_supported`, which is
the probe taken at startup. That is deliberate: installing `acpi_call` and
asking again should be a complete workflow, not one that needs a daemon
restart.

`getStatus.capabilities` does **not** ask the firmware on every read — a
status read is a poll, and a poll must not cost an ACPI round trip on the
file the fan cleaner writes through. What it does do is re-take the probe
when the *interface* facts have changed, all of which are a `stat`: whether
`hp-wmi` is there, whether `/proc/acpi/call` is there, and whether the
module is installed. That is precisely what moves while the daemon runs,
and freezing it at startup produced a real wrong sentence — `rgb get` went
on reporting "acpi_call is not installed" on a machine where it had been
installed since the daemon booted.

### Two things confirmed on hardware, both of them warnings

**`PASS` is not evidence that a dialect works.** On the OMEN this project
runs on, `lightbar` answers `PASS` to reads and writes, reports all four
zones black, and changes nothing; `fourZone` on the same machine returns
the keyboard's real colours and drives them. So a client must never present
"the firmware accepted it" as "it worked", and auto-selection puts
`fourZone` ahead of `lightbar` for exactly this reason.

**`acpi_call` truncates the reply.** It renders a buffer answer as the text
`{0x50, 0x41, …}` into a fixed result buffer of a few hundred bytes, so a
128-byte answer arrives as its first ~34. Zones 0-2 fit; zone 3 starts at
byte 34 and does not, so `readZones` reports it black however it was set.
The colour written to it is real. A short reply is read for what it
contains rather than failed outright — failing would send auto-selection to
the dialect that answers `PASS` and does nothing. `kernelZones` has no such
limit, which is the argument for preferring it.

### A missing `acpi_call` is `failed`, not `notCapable`

The one error mapping worth stating, because `notCapable` is the kind a
client is entitled to read as *"this machine will never do it, stop
offering the control"*:

| situation | kind |
|---|---|
| no `hp-wmi` on this machine | `unsupported` |
| `/proc/acpi/call` missing | `failed`, with the package name in the message |
| not root | `permissionDenied` |
| the firmware was asked and refused *this dialect* | `notCapable` |

A missing kernel module is one `pacman -S` away, and reporting it as a
permanent hardware limit would hide a working light strip behind an
install nobody was told to do.

### `/proc/acpi/call` cannot be read with `read_to_string`

Worth stating in the protocol document because it is a property of the
interface rather than of this code. `/proc/acpi/call` reports a size of
zero, like most of procfs, so `fs::read_to_string` has no hint to size its
buffer with and opens by probing with a very small one — and this interface
answers a small read with **nothing at all** rather than with the first few
bytes. The result is an empty string for a call the firmware answered.

That failure is silent and it lies: an empty reply is not `PASS`, so it is
reported as the firmware refusing, and a refusal reads as a verdict on the
machine. It is what made every lighting dialect and the fan cleaner report
"this machine cannot do it" on a machine that can. `pyren_core::acpi` reads
with one explicit large read; the reasoning is in a comment there, and
`dev/FINDINGS.md` has the measurements.

### `/proc/acpi/call` is one file, and the lock is not in this module

`acpi_call` is a *single global interface*: a call is a write followed by
a read of the same file, tied together by nothing. If a second process
writes in between, we read its answer and it reads ours. A short-lived CLI
gets away with that; a daemon with a control loop does not.

So every use in this process goes through `pyren_core::acpi::call`, which
holds one process-wide mutex across the write/read pair. It lives in
`core` rather than in this module because **more than one module needs
it**: the lightbar drives the light strip through it, and the fan cleaner
drives reverse spin through it. Two modules serialising against two
different mutexes would be two modules not serialising at all. They also
speak the same `SECU` buffer protocol, so `acpi::wmi_request` builds the
argument and `acpi::parse_bytes` reads the reply for both.

The lock is per *process*, which is the scope that is ours. Another
program on the machine using `acpi_call` at the same moment is outside it.

### Nothing here has been confirmed against a light strip

Every constant in the payload is upstream's reverse engineering, carried
across and unit-tested for **shape** only — the header fields, where the
four zones land, which replies count as success. The development laptop
has no `acpi_call` installed, so the firmware's own answer is the one
thing still untested. `rgb.getCapabilities` says in words which of the
three ways it is unavailable a given machine is in, which is what makes
that a hand-off rather than a mystery.

The daemon does **not** `modprobe` at startup. Probing is a question, and
a question should not change the answer; the module is loaded only on a
call that needs it, and only when running as root.

## `gpu` module

Which GPU is driving the screen — iGPU only, hybrid, or the discrete card
— read and written through the patched `hp-wmi` driver's own
`gpu_mux_mode`, not `supergfxctl`.

| method | params | result |
|---|---|---|
| `gpu.getStatus` | none | `{ "supported": bool, "mode": string \| null, "raw": number \| null }` |
| `gpu.setMode` | `{ "mode": "integrated" \| "hybrid" \| "discrete" \| "optimus" }` | as `getStatus` |

Confirmed on hardware, 2026-09-04: `gpu.getStatus` reads `hybrid` off the
development machine's `gpu_mux_mode`, and `gpu.setMode discrete` wrote `1`
to the same file and read `discrete` straight back — put back to `hybrid`
immediately afterward, since the switch only takes effect at the next
logout or reboot and nobody had asked for discrete to actually stick.
Nobody has yet sat through that logout/reboot to confirm the driving card
itself changes; that is the one thing left to try deliberately, with
`pyren-ctl gpu set`, when discrete is wanted for real.

### Why this is not a `supergfxctl` wrapper

`driver/hp-wmi-omen/hp-wmi.c` — the driver this project already patches
and installs for fan control — exposes
`/sys/devices/platform/hp-wmi/gpu_mux_mode` as a plain `RW` attribute that
talks to `HPWMI_GRAPHICS_MUX_QUERY` over ACPI-WMI directly, on every board
that reaches this driver at all. Where that file exists, wrapping a
second daemon to do the same round trip would only add a dependency and a
second place for the mode to disagree with what this one just set.

### Four modes, one small integer, read and write agree

The kernel source defines `HPWMI_MUX_MODE_*` as bits (`hybrid = BIT(1)`,
`discrete = BIT(2)`, …), which reads like the wire format — it is not. That
encoding exists only inside the driver, to check a requested mode against
a supported-set query before writing it. The byte actually read from and
written to `gpu_mux_mode` is a plain index: `0` hybrid, `1` discrete, `2`
optimus (NVIDIA render offload, distinct from plain "discrete" in the
firmware's own vocabulary), `3` uma (integrated only, called `integrated`
here since that is what choosing it means to a person). `gpu.setMode`
also accepts the app's own `GpuMode` names (`igpu`/`dgpu`/`uma` as
synonyms), so a client never has to know which vocabulary it is speaking.

### There is no userspace query for what a board supports

The capability check happens inside the kernel, on write, against a
design-data query that has no sysfs file of its own. So unlike `rgb` —
which can probe every dialect with a read that changes nothing — this
module cannot list what a board offers without asking it to switch. A
write the firmware refuses comes back `EOPNOTSUPP`, which `setMode`
reports as `notCapable` naming the mode, rather than as a bare I/O
failure.

## `network` module

System-wide smart queuing on the default-route interface — **not**
per-application traffic control. See `daemon/crates/network/src/lib.rs` for
why the app's per-process priority/block table has nothing behind it: it
needs per-process traffic accounting (cgroups/nftables/eBPF) this project
does not implement, flagged as the larger and less valuable half of
`dev/TODO.md` §2.1.

| method | params | result |
|---|---|---|
| `network.getStatus` | none | `{ "supported": bool, "interface": string \| null, "mode": "off" \| "auto", "activeQdisc": string \| null }` |
| `network.setMode` | `{ "mode": "off" \| "auto" }` | as `getStatus` |

`off` deletes the interface's root qdisc, handing it back to the kernel's
own default. `auto` replaces it with `cake`, falling back to `fq_codel` on
a kernel with no `sch_cake`; both fair-queue by flow, which is what keeps a
game or a call responsive while something else saturates the link, with no
need to know which process owns which packet.

`mode` is this daemon's own memory of the last `setMode` call, not a read
of the interface — `fq_codel` is already several distributions' own
`net.core.default_qdisc`, so seeing it active proves nothing about who set
it. It resets to `off` on daemon restart. `activeQdisc` is the separate,
honest read of `tc qdisc show` — ours or not.

## `overclock` module

| method | params | result | status |
|---|---|---|---|
| `overclock.getState` | none | every GPU, what can be moved on it, and what is set | ✅ implemented, read-only |
| `overclock.probe` | `{ "allowWrites"?: bool }` | a **fresh** look, replacing the startup one | ✅ implemented; writes only with `allowWrites` |
| `overclock.setConsent` | `{ "accepted": bool }` | the state | ✅ implemented |
| `overclock.apply` | `{ "gpu"?, "coreOffsetMhz"?, "memOffsetMhz"?, "clockLock"?, "holdSecs"? }` | the state, with `pending` armed | ✅ implemented; clock locks need root, offsets need Coolbits |
| `overclock.confirm` | none | the state | ✅ implemented |
| `overclock.cancel` | none | the state | ✅ implemented |
| `overclock.reset` | `{ "gpu"? }` | the state | ✅ implemented |
| `overclock.setRestoreOnStart` | `{ "enabled": bool }` | the state | ✅ implemented |

This is the one module that can leave the envelope the firmware shipped,
and the only one whose failure mode is not an error message: an offset
that survives a benchmark can still hang a machine in a game, and what
happens then is a frozen screen and lost work. So three things stand
between a slider and a clock, and a client cannot skip any of them.

### 1. Consent, in the daemon's words

`getState.consent.text` is the warning. It is served by the daemon rather
than written into the app on purpose — what somebody agreed to should not
be something a client can reword — and `setConsent` records that it was
accepted, with a version stamp so a future rewording stops counting.
Until then `apply` refuses with `invalidParams`. `reset` never does:
**there is no state of this module in which "put it back" is refused.**

Withdrawing consent (`accepted: false`) is not a preference change; it
puts every card this daemon has moved back to stock.

### 2. A climb, not a write

An apply is walked in steps — 15 MHz of core, 50 MHz of memory at a time —
and each step is written, read back and re-queried before the next. That
does **not** find a stable offset; only a workload can do that. What it
buys is a bounded distance between "the card was answering" and "the card
stopped", so a failure names the value that caused it and the card is put
back to where it started rather than left at whichever step died.

Nothing is ever asked for that the driver has not advertised: the ranges in
`coreOffset` / `memOffset` / `clockLock` are read from the hardware, a
request outside them is clamped, and the clamp is reported in `note`
rather than applied silently.

`getState`'s prose fields — every GPU's `detail`, the top-level `detail`,
`note` and `error` — are **`Msg` objects** (see *Translatable messages*
above), not bare strings: `{ key, params?, text }`. A `notCapable` /
`failed` refusal from `apply` likewise carries `key`/`params` beside its
`message`. `consent.text` stays a bare string — it is a legal record, not a
UI label, and the app shows a translation of its own keyed on the version.

### 3. A revert timer, disarmed by hand

`apply` arms `pending`, and the daemon undoes the change by itself when
`secondsLeft` runs out. `confirm` keeps it; `cancel` undoes it early. Both
paths do the *same* revert, because "the timer ran out" and "the user
pressed undo" must not be two pieces of code that can disagree.

The case this exists for is the one where **no further call arrives**: the
desktop is gone, the app is gone, and the only thing still running is a
root daemon with a thread and a deadline. A client that closes mid-
countdown has not kept an overclock — it has arranged for one to be undone.

That timer also spans reboots. The armed flag is persisted, so a daemon
that starts and finds it still set knows the machine went away while
overclocked; on such a boot `restoreOnStart` is ignored, nothing is
written, and `unconfirmedAtStart` says so.

### What each machine can actually do is probed

| vendor | mechanism | needs | status |
|---|---|---|---|
| NVIDIA | `nvidia-settings` clock offsets | an X display whose screen has `Coolbits` | implemented |
| NVIDIA | `nvidia-smi --lock-gpu-clocks` | root | implemented |
| AMD | `pp_od_clk_voltage` (Overdrive) | `amdgpu.ppfeaturemask` | **detected, not driven** |
| Intel | `gt_max_freq_mhz` | — | nothing to overclock: it is a ceiling, not an offset |

Which of these a laptop has is decided by the driver version, the session
and the X configuration far more than by the model of the card, so all of
them are probed and none is looked up — the same rule as
§"`controls` and `compatibility` are measured, not looked up".

Two of the rows deserve their reasons in full:

- **A clock lock is not an overclock.** `--lock-gpu-clocks` cannot ask for
  a frequency the card was not shipped able to run. It is here because it
  is the knob that decides how long the card is willing to *stay* there,
  which is what somebody on this page is usually after — and on the laptop
  this was written on it is the only mechanism that works at all.
- **AMD Overdrive is detected and deliberately not driven**, for the same
  reason `rgb` probes the per-key keyboard without driving it: there is no
  AMD machine to test on, and a wrong write to `pp_od_clk_voltage` does not
  fail with an error message. `detail` says so in words.

### What was found on the development laptop

Driver 610.57.04, RTX 5060 Laptop GPU, Wayland session with XWayland:

- both offset attributes **read** fine, advertising -1000..1000 MHz for the
  core and -2000..6000 for the memory transfer rate;
- writing one back at its *current* value — a no-op assignment, and the
  only way to tell a readable attribute from a settable one — is refused
  with "The current user does not have permission for operation", which is
  what a screen with no `Coolbits` says.

So the offsets are visible and not settable here, and
`overclock.probe --write` is what turns that from a guess into a sentence.

Run **as root** - which is how the daemon runs in production - it fails one
step earlier and for a different reason: a daemon started by systemd is in
nobody's session, so the X server answers *"Authorization required"* before
`Coolbits` ever comes up. On a Wayland desktop there is not even a cookie to
hand it, because the compositor starts `Xwayland` with no `-auth` file at
all (Hyprland: `Xwayland :1 -rootless -core -listenfd … -wm …`) and the
server falls back to admitting the uid that owns it. So the offsets are
reachable by *that user's* processes and by nothing else, and the module
says which of the three refusals it got rather than passing the driver's
wording through. The fixes it names are the two that exist: the user
allowing us in from inside their session (`xhost +si:localuser:root`), or
the operator pointing the daemon at a display it may open, with
`PYREN_X_DISPLAY` and `PYREN_XAUTHORITY`.

**The clock lock, in contrast, is proven against the hardware.** As root,
900-1200 MHz took the idle card from 180 MHz / P8 / 7.5 W to 892 MHz / P5 /
9.9 W, `getState` reported the pending change, and letting the confirmation
lapse put the card back at 180 MHz / P8 by itself - the revert timer doing
exactly what it exists for, on a real GPU.
When `offsetsWritable` comes back `false` the offset ranges are withdrawn
from the state as well: a slider that can only ever fail is worse than no
slider.

### What this module will not do

- **Raise the CPU's package power limits.** It belongs behind this same
  consent (`dev/TODO.md`), and it is not here because the `power` module
  owns those registers and re-applies them, clamped to stock, on every mode
  change. Two owners on one register is a worse bug than a missing feature.
- **Undervolt**, or set a fan curve of its own. More heat is the `fan`
  module's problem, and it is already good at it.

## `hotkey` module

The laptop's own performance key — Fn+P on an OMEN — heard by the daemon.

| method | params | result |
|---|---|---|
| `hotkey.getStatus` | none | what is bound, what is being watched, and why nothing is if nothing is |
| `hotkey.learn` | `{ "timeoutMs"?: number, "bind"?: bool }` | `{ "press": { device, keycode, scancode, modifiers, describe, label } \| null, "timedOut", "bound" }` |
| `hotkey.setTriggers` | `{ "triggers": [{ "device"?, "keycode"?, "scancode"?, "modifiers"? }] }` | as `getStatus` |
| `hotkey.setEnabled` | `{ "enabled": bool }` | as `getStatus` |
| `hotkey.press` | none | `{ "fired": true }` — runs the action without the key |

`getStatus` also carries `label`: the bound shortcut written the way a
person would, `"Ctrl+Alt+P"`, or `null` when nothing is bound. It is for
showing, not for parsing — `triggers` is the machine-readable form.

`getStatus.detail` — the one sentence that says why nothing happens when
nothing does (not root, no key bound, switched off) — is a **`Msg` object**
(see *Translatable messages* above), and a `setTriggers` refusal carries
`key`/`params` beside its `message`. `label` and `press.describe` stay bare
strings: a shortcut name and a `keycode N` diagnostic are not translated.

### A shortcut can be a combination

`modifiers` is `{ ctrl, shift, alt, meta }`, all four booleans, and it is
matched **exactly**: `Ctrl+P` does not fire on `Ctrl+Shift+P`. A shortcut
that swallowed its own supersets would hijack whatever the user had bound
there. Left and right are the same modifier — nobody means "the right
Shift specifically" — and `KEY_RIGHTALT` (AltGr) counts as Alt.

A modifier is never a shortcut on its own. `setTriggers` refuses one, and
the watcher never reports one: a modifier going down updates what is held
and produces no press. That is what makes learning a combination work at
all — the modifier necessarily goes down first, so a learn window that
took the first key to arrive would bind `Ctrl` every single time.

Omitting `modifiers` means "none held", which is also how a config file
written before this existed is read. That is the right reading of it: the
vendor key such a file binds is pressed on its own.

**Why the daemon and not the compositor.** A combination is exactly what a
desktop keybinding could do, and this deliberately does not use one. The
daemon hears `/dev/input` directly, so one shortcut works on Hyprland,
GNOME, KDE and X11 alike, keeps working at the login screen before any
session exists, and is configured in one place instead of per-desktop.

### Nothing is bound by default

There is no table of "the OMEN key is keycode N" in this daemon. Which key
a laptop sends, whether the kernel has a keycode for it at all, and which
device it arrives on vary between machines of the *same model* — and a
guessed table is precisely the mistake this project already made once with
board ids (see "measured, not looked up", above). So the machine is asked:
`hotkey.learn` opens a few seconds, the user presses their key, and
whatever arrives is bound.

`hotkey.learn` holds the connection open until a key arrives or the timeout
expires. A timeout is **not** an error — `{ "press": null, "timedOut": true }`
is the honest description of a laptop whose key never reaches Linux at all,
which is a real hardware answer and not a failure of this call.

### A button is not a key

The watcher opens only devices that report at least one keycode below
`BTN_MISC` (0x100), read from sysfs `capabilities/key`. A mouse and a
touchpad report buttons and no keys at all, so neither is ever opened, and
`setTriggers` refuses a `BTN_*` keycode besides.

Both halves exist because of one accident. A learn window caught
`BTN_TOOL_FINGER` (325) from the touchpad - the kernel's way of saying "a
finger is resting here" - and bound the power-mode cycle to *touching the
trackpad*, which then walked the machine through all four modes as fast as
it could be touched. Nothing about that was a key press.

The ranges are not a threshold: the kernel came back for more `KEY_*`
codes above the first block of buttons, so `0x100..0x160` and
`0x2c0..0x300` are buttons and everything else is a key.

### A key with no keycode is still a key

Two shapes arrive from `/dev/input`:

- A key the kernel has a keycode for: `EV_MSC/MSC_SCAN` then `EV_KEY`.
- A key it does not: the scancode alone, and nothing else. On the test
  laptop that is `atkbd: Unknown key pressed ... code 0xab`.

The second is bindable here by its `scancode`, which means **no
`setkeycodes` and no udev hwdb entry** — the daemon changes nothing about
the system to hear the key, and because the compositor never sees such a
key either, there is nothing for it to collide with.

Its one cost is in `repeatGuardMs`: an unmapped key reports the *same* bare
scancode when it goes down and when it comes up, with nothing to tell the
two apart, so without a short coalescing window one press would advance two
modes. It defaults to 300 ms.

### `permissionDenied`, not `unsupported`

Reading `/dev/input` needs root. An unprivileged daemon therefore reports
`supported: true` — the keyboard is there — and refuses `learn` with
`permissionDenied`, whose fix is the systemd unit. Reporting the feature as
absent would hide the fix along with it.

### What this module does not decide

It does not know what a hotkey *does*. The daemon binary hands it an action
at startup; deciding that the shortcut shows the power modes is
coordination between two modules, and modules here never call each other.

### Privacy

This is a root process reading keyboards, so it is worth being explicit: a
key that matches no trigger is compared and dropped on the spot — never
stored, never logged, never sent anywhere. The one exception is a learn
window the user opened deliberately, which lasts seconds and reports
exactly one press: the one they pressed to answer the question.

## `keymap` module

Remaps one physical key to another, system-wide, with no compositor
keybinding involved. Closes the "key mapping" half of `dev/TODO.md` §2.1 —
the backend decision it names (`keyd`, a `udev` hwdb entry, or an
evdev-level remapper) is the third: this daemon already opens
`/dev/input/event*` directly for `hotkey`, so a `/dev/uinput` virtual
device reusing that same access is one fewer moving part than a second
daemon's config file.

| method | params | result |
|---|---|---|
| `keymap.getStatus` | none | `{ "enabled", "running", "detail", "devices": [string], "mappings": [{ "from": { "device"?, "keycode" }, "to" }] }` |
| `keymap.setMapping` | `{ "from": { "device"?: string, "keycode": number }, "to": number }` | as `getStatus` |
| `keymap.removeMapping` | `{ "device"?: string, "keycode": number }` | as `getStatus` |
| `keymap.setEnabled` | `{ "enabled": bool }` | as `getStatus` |

Keycodes are Linux evdev codes (`KEY_A` = 30, and so on) — the same
vocabulary `hotkey.learn`'s `press.keycode` already speaks — not DOM
`KeyboardEvent.code` strings; a client translates once, at the UI layer,
rather than the daemon accepting two vocabularies for the same number.

`getStatus.detail` is a **`Msg` object** (see *Translatable messages*
above); `setMapping`/`removeMapping`/`setEnabled` refusals carry
`key`/`params` beside `message` the same way `hotkey`'s do.

### Grabbing a keyboard silences `hotkey` on it

`EVIOCGRAB` makes this module's file descriptor the *only* one the kernel
delivers a device's events to, `hotkey`'s own reader included. So enabling
a mapping on the keyboard the vendor performance key lives on stops
`hotkey` hearing it for as long as the remapper runs on that device. This
is the reason `enabled` defaults to `false` here, unlike `hotkey`'s own
`enabled` (which only ever gates whether a heard key does anything, never
whether the key is heard at all): a keymap must be turned on deliberately,
after a mapping exists, not the moment the daemon starts.

### One virtual keyboard, not one per device

Every grabbed keyboard's events are forwarded through a single `uinput`
device, substituting a keycode when a mapping matches. A mapping with no
`device` matches that keycode from any of them; naming a `device` scopes it
to one, which round-trips through `getStatus` but is not surfaced
differently by the merge — the common case, one keyboard, does not need it.

Only a bare keycode is substituted — no chords, no macros. Those stay the
app's own "coming soon", because they are a sequence of synthetic events
this module has no reason to own once a plain substitution is what the
virtual device is for.

### Not yet run against hardware

Grabbing this development machine's own keyboard from inside the same
session it is being edited in is not a test to run blind — a mistake in
the substitution table would take the keyboard away from whoever needs it
to fix that mistake. The ioctl numbers (`EVIOCGRAB`, `UI_SET_EVBIT`,
`UI_SET_KEYBIT`, `UI_DEV_SETUP`, `UI_DEV_CREATE`, `UI_DEV_DESTROY`) are
derived from the same `_IOC` formula the kernel headers use and checked
against the values those headers are known to produce
(`crates/keymap/src/raw.rs` tests); what a real grab-and-remap run on a
spare keyboard, or over SSH, would still confirm is untested here.

## Adding a new module

1. New crate under `daemon/crates/<name>`, depending on `pyren-core`,
   implementing `Module`.
2. Register it in `daemon/daemon/src/main.rs` (`registry.register(Box::new(...))`).
3. Document its methods in a new table in this file.
4. Add matching `#[tauri::command]` wrappers in `app/src-tauri/src/lib.rs`
   only for the methods the frontend actually calls — don't blanket-proxy
   every module method through Tauri commands speculatively.
