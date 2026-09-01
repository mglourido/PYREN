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
