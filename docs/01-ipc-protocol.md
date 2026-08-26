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
