# IPC protocol (app ⟷ daemon)

The Tauri app and `omen-hub-daemon` are separate OS processes (see
`00-design-plan.md` for why). They talk over a Unix domain socket using a
small JSON-RPC-like protocol. This document is the source of truth for that
wire format — keep it in sync with `daemon/crates/core/src/lib.rs` (server)
and `app/src-tauri/src/lib.rs` (client) if either changes.

## Transport

- Unix domain socket, `SOCK_STREAM`.
- Path: `$OMEN_HUB_SOCKET`, falling back to `/tmp/omen-hub-daemon.sock` for
  unprivileged local development. Production (daemon running as a root
  systemd service) should set `OMEN_HUB_SOCKET=/run/omen-hub/daemon.sock`
  and lock down that directory's permissions once auth beyond "can open
  this socket" is needed.
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
{ "id": 1, "error": "human-readable message" }
```

`error` is always a plain string (module-side errors, e.g.
`omen_hub_core::ModuleError`, are converted with `.to_string()` at the
boundary) — there is no structured error code yet. If callers need to
branch on error *kind* later (e.g. "needs privilege" vs "unsupported
hardware" vs "bad params"), that should become a structured `{ kind, message }`
object rather than string-matching — don't build string-matching error
handling against today's messages.

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
  "compatibility": "unsupported",
  "supported": false,
  "reason": "ASUS is not an HP machine; monitoring works, OMEN hardware control does not"
}
```

Fields are read from `/sys/class/dmi/id`, `/proc/cpuinfo` and `lspci`.
Firmware placeholder strings (`"To be filled by O.E.M."`, `"System Product
Name"`) are reported as `null` rather than shown to the user verbatim.

`compatibility` is one of:

| value | meaning |
|---|---|
| `supported` | HP board present in the known-good list (`daemon/crates/system/src/boards.rs`) |
| `untested` | HP OMEN/Victus machine whose board isn't listed — fan control may still work, so the UI **warns rather than blocks** |
| `unsupported` | not an HP gaming machine; monitoring works, hardware control won't |

The board list is advisory only, exactly as in the Python original — it
never gates functionality, only the warning the UI shows.

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
             "powerW": 169.5, "clockMhz": 1837.0 }],
  "processes": [{ "pid": 32556, "name": "re4.exe", "cpuPercent": 41.8, "memMb": 2847.0 }]
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
- `processes` is the busiest 12; `cpuPercent` is a share of the **whole
  machine** (0–100), not of one core.
- `gpus` uses `nvidia-smi` for NVIDIA cards and DRM sysfs
  (`gpu_busy_percent`, `mem_info_vram_*`) for others. Fields a driver
  doesn't expose are `null` — i915/xe report name only.
- Virtual network interfaces (`lo`, `veth*`, `docker*`, `br-*`, `virbr*`,
  `vnet*`, `tap*`) are excluded so container traffic isn't counted twice.

## `power` module

The Eco / Balanced / Performance / Unlimited switch, plus the background
supervisor that can drive it automatically.

| method | params | result | status |
|---|---|---|---|
| `power.getState` | none | current mode, backend state, battery, supervisor config | ✅ implemented |
| `power.setMode` | `{ "mode": "eco" \| "balanced" \| "performance" \| "unlimited" }` | `{ "applied": [...], "failed": [...] }` | ✅ implemented |
| `power.setAutoConfig` | full auto config object | stored config + whether it reached disk | ✅ implemented |
| `power.setRestoreOnStart` | `{ "enabled": bool }` | as above | ✅ implemented |

### Mechanisms

`setMode` tries, in order of how directly it maps to what the OMEN Gaming
Hub does:

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

| field | meaning |
|---|---|
| `enabled` | master switch |
| `ecoOnBattery` | drop to Eco whenever running on battery (beats the load rule) |
| `performanceOnLoad` | step up to Performance under sustained load |
| `loadHigh` / `loadLow` | 1-minute load average **per core** above/below which load counts as high/low. The gap between them is a dead band where the supervisor has no opinion — this is what stops the mode flapping around a threshold. |
| `samplesToSwitch` | consecutive agreeing samples required before switching |
| `intervalSecs` | how often it samples |
| `manualOverrideSecs` | how long a manual `setMode` suspends the supervisor — whoever is at the keyboard wins |

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
`omen_logic.py`. Split into **inspect → plan → apply** rather than one
imperative script: installing means unloading a kernel module, replacing a
file under `/lib/modules` and regenerating the initramfs, so the user
should see exactly what will run before authorising it — and a rendered
plan is also something that pastes into a bug report.

| method | params | result | status |
|---|---|---|---|
| `installer.inspect` | none | what this machine has, and whether the patch is needed | ✅ implemented |
| `installer.plan` | `{ action, preferHooks?, force? }` | ordered steps, blockers, warnings | ✅ implemented |
| `installer.apply` | as above plus `confirm`, `cpuMaxRpm`, `gpuMaxRpm`, `experimentalBoard`, `boardTable` | `{ plan, report }` | ⚠️ implemented, **execution untested** |

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
their changes by hand. `inspect` looks for it in `$OMEN_HUB_DRIVER_DIR`,
then `/usr/share/omen-hub/driver`, then a sibling checkout, and reports a
`no-driver-source` blocker when it finds none.

## `fan` module

| method | params | result | status |
|---|---|---|---|
| `fan.getStatus` | none | `{ "driverInstalled": bool, "cpuTempC": number \| null, "fanRpm": number, "isReverse": bool }` | ✅ implemented, read-only, no privileges needed |
| `fan.setMode` | TBD | — | ❌ not implemented (see `daemon/crates/fan/src/lib.rs`) |
| `fan.setCurve` | TBD | — | ❌ not implemented |

`cpuTempC` is `null` if no CPU temperature sensor was found (mirrors the
Python original's fallback chain: `coretemp`/`k10temp` hwmon →
`thermal_zone0` → give up). `fanRpm` is `max(fan1, fan2)`, decoded through
the hp-wmi reverse-bit encoding — see
`../omen-fan-control-main/docs/02-kernel-driver.md` for why raw values
`>= 12800` get remapped.

## Adding a new module

1. New crate under `daemon/crates/<name>`, depending on `omen-hub-core`,
   implementing `Module`.
2. Register it in `daemon/daemon/src/main.rs` (`registry.register(Box::new(...))`).
3. Document its methods in a new table in this file.
4. Add matching `#[tauri::command]` wrappers in `app/src-tauri/src/lib.rs`
   only for the methods the frontend actually calls — don't blanket-proxy
   every module method through Tauri commands speculatively.
