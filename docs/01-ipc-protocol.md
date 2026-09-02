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

Note that `system` always reports `supported: true` (any Linux machine can
report its own vitals). Whether *OMEN hardware control* is possible is a
different question, answered by `system.getInfo`'s `compatibility` field.

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
  "reason": "this machine accepts: power modes",
  "privileges": { "root": true, "perfEvents": true }
}
```

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

It is **two systems**, matching the two switches on the app's home screen:

| | when it acts | what it does |
|---|---|---|
| `ecoOnBattery` | the machine is unplugged | drops to Balanced *at once*, then to Eco if it stays idle or the battery gets low |
| `performanceOnLoad` | the machine is plugged in | steps up to Performance *at once*, then back to Balanced if it sits idle |

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

| field | meaning |
|---|---|
| `enabled` | master switch |
| `loadHigh` / `loadLow` | 1-minute load average **per core** above/below which load counts as high/low. The gap between them is a dead band where the supervisor has no opinion — this is what stops the mode flapping around a threshold. |
| `batteryLowPercent` | at or below this charge, Eco is preferred on battery whatever the load is doing |
| `samplesToSwitch` | consecutive agreeing samples required before a *refinement*. Transitions ignore it. |
| `intervalSecs` | how often it samples |
| `manualOverrideSecs` | how long a manual `setMode` suspends refinement — whoever is at the keyboard wins |

The load average is used rather than instantaneous CPU usage precisely
because it is already smoothed: a mode switch spins fans up or down and is
very visible, so only *sustained* load should trigger one.

`getState` also reports `autoOverrideSecondsLeft` and `lastAutoSwitch` so
the UI can explain why the supervisor is or isn't acting.

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

> **This is no longer the recommended path for the driver.** Manual fan
> control is upstream in recent kernels, so installing a patched
> out-of-tree driver is usually a downgrade. Use `fan.diagnose` to verify
> what the running kernel already does; the driver actions here remain for
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
| `installer.plan` | `{ action, preferHooks?, force? }` | ordered steps, blockers, warnings | ✅ implemented |
| `installer.apply` | as above plus `confirm`, `cpuMaxRpm`, `gpuMaxRpm`, `experimentalBoard`, `boardTable` | `{ plan, report }` | ⚠️ implemented, **execution untested** |

All three are driven from the app by `DriverWizard.svelte` at the bottom
of `/drivers`, which renders the plan's steps and their commands and keeps
"apply" disabled until a dry run of those exact options has come back —
see `docs/03-frontend.md`. `pyren-ctl` has no installer subcommand; the
wizard and `--install-service` are the two ways in.

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

### Driver sources are not bundled

`hp-wmi.c` is a modified copy of a GPL-2 kernel driver maintained in the
`omen-fan-control` project; carrying a fork of it here would mean tracking
their changes by hand. `inspect` looks for it in `$PYREN_DRIVER_DIR`,
then `/usr/share/pyren/driver`, then a sibling checkout, and reports a
`no-driver-source` blocker when it finds none.

## `fan` module

| method | params | result | status |
|---|---|---|---|
| `fan.getStatus` | none | the status object below | ✅ implemented, read-only, no privileges needed |
| `fan.diagnose` | `{ "allowWrites": bool }` | full self-test report (see below) | ✅ implemented |
| `fan.setMode` | `{ "mode": "auto"\|"max"\|"manual"\|"curve", "pwm"?: 0-255 }` | the status object | ✅ implemented, needs root |
| `fan.setCurve` | `{ "curve": [{ "tempC": number, "percent": number }], "interpolation"?: "smooth"\|"discrete" }` | the status object | ✅ implemented |
| `fan.setRestoreOnStart` | `{ "enabled": bool }` | the status object | ✅ implemented |
| `fan.calibrate` | `{ "seconds"?: 10-120 }` | the calibration report below | ✅ implemented, needs root, **blocks and spins the fans** |

Every write returns the same status object `getStatus` does, so a caller
never has to follow a write with a read:

```json
{
  "driverInstalled": true,
  "capabilities": { "switchMode": true, "setSpeed": false },
  "cpuTempC": 38,
  "fanRpm": 2097,
  "isReverse": false,
  "mode": "auto",
  "pwm": null,
  "targetPwm": null,
  "manualPwm": 128,
  "curve": [{ "tempC": 40, "percent": 20 }],
  "interpolation": "smooth",
  "restoreModeOnStart": false,
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

### `fan.diagnose`

The fan-control self-test. This is the project's answer to "is the driver
right?", and it replaced installing one: manual fan control is upstream in
recent kernels, so on most machines the right answer is *the stock driver
already does this*, and the useful thing is to prove it rather than replace
it.

```json
{
  "verdict": "fullControl" | "monitoringOnly" | "unsupported",
  "summary": "…",
  "driverNotice": "…" | null,
  "wroteToHardware": false,
  "checks": [
    { "id": "pwm1", "title": "PWM channel", "status": "pass",
      "detail": "pwm1 = 128 (0-255)", "remedy": null }
  ]
}
```

Checks cover the hp-wmi platform device, the hwmon node, both fan inputs
(decoding the reverse-spin encoding rather than reporting a 15200 "rpm"),
`pwm1`, `pwm1_enable` and what its value means, the ACPI platform profile,
the CPU temperature sensor and `acpi_call`. Each carries a `remedy` when
there is something to do about it.

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

`cpuTempC` is `null` if no CPU temperature sensor was found (mirrors the
Python original's fallback chain: `coretemp`/`k10temp` hwmon →
`thermal_zone0` → give up). `fanRpm` is `max(fan1, fan2)`, decoded through
the hp-wmi reverse-bit encoding — see
`docs/02-kernel-driver.md` in the `omen-fan-control` project (see
`dev/README.md` for where that checkout is) for why raw values
`>= 12800` get remapped.

## Adding a new module

1. New crate under `daemon/crates/<name>`, depending on `pyren-core`,
   implementing `Module`.
2. Register it in `daemon/daemon/src/main.rs` (`registry.register(Box::new(...))`).
3. Document its methods in a new table in this file.
4. Add matching `#[tauri::command]` wrappers in `app/src-tauri/src/lib.rs`
   only for the methods the frontend actually calls — don't blanket-proxy
   every module method through Tauri commands speculatively.
