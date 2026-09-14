# Debug logging system — design

Date: 2026-09-14

## Problem

Today, diagnosing a problem on a user's machine means asking them to reproduce
it while reading `journalctl` at `PYREN_LOG=debug` — which is fine for "the
daemon printed something wrong" but useless for anything that happened before
anyone thought to look, or that depends on a sequence of state (fan mode
history, what the RGB daemon actually sent, what a calibration run measured
at each step). There is no record a user can hand back for a bug report.

This adds an opt-in, structured debug-logging system: off by default (so it
costs nothing on a machine nobody is diagnosing), togglable from one setting
in the app, and — once on — keeps a rolling history of what the daemon saw,
decided and did, plus the same for the OSD widget and the desktop app, in
plain JSONL files a user can read or attach to an issue.

## Non-goals

- Not a replacement for `PYREN_LOG` / `pyren_core::log`. That stays as the
  developer-facing stderr/stdout logger for the systemd journal. This system
  is user-facing, opt-in, and file-based.
- No log shipping, no telemetry to any server. Everything stays on disk,
  local to the machine.
- No per-category toggles. One switch, "Registros de depuración", turns all
  of this on or off together.
- No export/zip bundling in this iteration. A "open logs folder" button is
  enough; packaging a bug-report bundle can be a later, separate addition.
- No redaction pipeline. Nothing that flows through the IPC protocol today is
  a secret (see docs/01-ipc-protocol.md) — no passwords, no tokens — so
  logging request/response bodies verbatim is not a new exposure.

## Storage layout

The daemon normally runs as root under systemd (see
`docs/01-ipc-protocol.md` §Transport), so its own `$HOME` is not the desktop
user's. Writing to `~/.cache` on a root process would either fail or land in
`/root/.cache`, which the user could never find and the app could never read
without a privilege escalation for every poll. So the root belongs to
whichever process is actually running as the user:

| Writer | Process identity | Root directory |
|---|---|---|
| Daemon (systemd, root) | root | `/var/cache/pyren/depuration`, created `0750`, group `pyren` |
| Daemon (dev, `cargo run` unprivileged) | the developer | `~/.cache/pyren/depuration` |
| OSD widget | the desktop user | `~/.cache/pyren/depuration` |
| Desktop app (Tauri) | the desktop user | `~/.cache/pyren/depuration` |

This mirrors `pyren_config::ConfigStore::system()`'s existing fallback
(`daemon/crates/config/src/lib.rs`): prefer the system location, fall back to
the per-user one only when the system one isn't writable, and decide by
actually trying to create it rather than checking the effective uid.

`/var/cache/pyren/depuration` being group-`pyren`-readable at `0750` reuses
the exact trust boundary the socket already uses: anyone who can talk to the
daemon (group member) can already read its logs directly off disk, no new
IPC method needed to fetch log content. Same override knob as config:
`PYREN_DEPURATION_DIR` for tests and unusual setups.

Because the daemon's directory and the desktop processes' directory are
genuinely different paths in the systemd case, the app's "open logs folder"
button (below) opens **its own** `~/.cache/pyren/depuration` (widget +
frontend logs, always readable) and, when the daemon's directory differs and
is group-readable, offers a second link to it.

## Files

One append-only JSONL file per category — one JSON object per line, newest
last, so `tail -f` and standard tools work without special parsing:

| File | Written by | Content |
|---|---|---|
| `driver-kernel.jsonl` | daemon | Driver/kernel identity, one entry per detected change |
| `performance.jsonl` | daemon | Power mode + fan mode/curve, combined, one stream |
| `lighting.jsonl` | daemon | RGB daemon calls: mode, colors, brightness, raw bytes sent |
| `calibration.jsonl` | daemon | One entry per `fan.calibrate` / `fan.diagnose` / `fan.probeSpeedControl` run, full detail |
| `cleaner.jsonl` | daemon | Reverse fan mode: start/stop/status |
| `installer.jsonl` | daemon | Driver installer actions (`installer.apply`, `plan`, `inspect`) |
| `daemon.jsonl` | daemon | Daemon lifecycle: startup, shutdown, module init failures |
| `ipc.jsonl` | daemon | Every IPC call: module, method, params, ok/error, duration — the general transcript |
| `widget.jsonl` | osd | Widget errors and mode changes it displayed |
| `frontend.jsonl` | app (Tauri) | Uncaught JS errors, explicit user-action breadcrumbs |

Each line carries at least `{"ts": <unix ms>, ...category-specific fields}`.
No cross-file correlation id is introduced in this iteration — each file
reads standalone, which is what "one history per topic" means in practice.

### Rotation

Each file is capped at 5 MB. On exceeding it, the file is renamed to
`<name>.jsonl.1` (overwriting any previous one) and a fresh file started.
One backup generation, no more — this is a diagnostic aid a user turns on
for a session or two, not a long-term audit log. Worst case footprint: 10
files × 2 generations × 5 MB ≈ 100 MB, acceptable for something opt-in.

## Toggle

One boolean, persisted daemon-side as its own config namespace `debug.json`
(via `pyren_config::ConfigStore::system()`, same mechanism as `power.json` /
`fan.json`), with an in-memory `AtomicBool` mirror so every call site's
"should I even build this line?" check is one atomic load — the actual cost
of this feature when switched off.

New IPC methods, `debug` module:

| method | params | result |
|---|---|---|
| `debug.getStatus` | none | `{ "enabled": bool, "daemonDir": string, "daemonDirWritable": bool, "userDir": string }` |
| `debug.setEnabled` | `{ "enabled": bool }` | same as `getStatus` |

Toggling publishes `debug.changed` on the `EventBus` (`{ "enabled": bool }`),
so the OSD and the app pick it up immediately without polling — same pattern
already used for `power.mode` / `fan.mode`.

`daemonDirWritable: false` is how the UI tells a user "the switch is on but
nothing is actually being written" instead of a silent no-op — the same
"reported rather than guessed" instinct as `power.json`'s `saveError`.

## Capture mechanism

Two chokepoints, not per-module instrumentation — consistent with the
existing rule that modules don't call each other and don't know who's
listening (see `events.rs`'s doc comment on `EventBus::subscribe`).

**Every call site is exactly one line: a call to a `debuglog::` function,
added after the existing logic, never a branch woven into it.** Nothing
inside `Registry::dispatch` or the `EventBus` wiring changes shape — an
`if`/`match` is not restructured to accommodate logging, a call is appended
where the value to log already exists (the `Response`, the published
event). Removing the feature later means deleting those one-line calls (and
the `debuglog` module itself); the surrounding function is exactly what it
was before. The same rule applies to the OSD and app call sites below.

### 1. `EventBus` subscriber

Alongside the existing `events.subscribe(...)` wiring (main.rs:330), one more
subscriber filters:

- `power.mode`, `fan.mode`, `fan.floorRaised` → appended to
  `performance.jsonl`, tagged with topic and full payload (mode, source,
  manualPwm, etc. — whatever the event already carries).

This is what makes the history genuinely complete: `EventBus::subscribe` is
called for *every* publish regardless of source (hotkey, the auto
supervisor, an external firmware-profile change, a plain `setMode`), unlike
the bounded ring `core.nextEvent` reads from. A change made while no client
was polling still lands in the file.

### 2. `Registry::dispatch` call site

`daemon/crates/core/src/lib.rs`'s `Registry::dispatch` (line 329) is the one
place every IPC request already passes through — one `match` against
`req.module`, one `m.call(...)`, one `Response` built from the result. This
design adds **one line** right before that `Response` is returned:
`debuglog::on_ipc(&req, &response, started.elapsed());` — a plain function
call, not a restructuring of the match. Timing the call needs one
`let started = Instant::now();` at the top of the function; nothing else in
`dispatch` changes. `debuglog::on_ipc` itself (in `pyren_core::debuglog`,
not in `dispatch`) does all the branching:

1. When enabled, always appends a one-line summary to `ipc.jsonl`:
   `{ ts, module, method, ok, durationMs, error? }` (params included, since
   nothing in the protocol is secret).
2. Additionally, based on `(module, method)`, appends a richer copy to the
   matching category file — the existing result/report struct is already
   `Serialize`, so this is "dump what the module already computed", not new
   instrumentation inside `fan`/`rgb`/`installer`:

   | module.method | → file |
   |---|---|
   | `fan.calibrate`, `fan.diagnose`, `fan.probeSpeedControl` | `calibration.jsonl` |
   | `fan.startCleaning`, `fan.stopCleaning`, `fan.cleanerStatus` | `cleaner.jsonl` |
   | `rgb.*` | `lighting.jsonl` |
   | `installer.apply`, `installer.plan`, `installer.inspect` | `installer.jsonl` |

   This is why `fan.diagnose`'s and `fan.calibrate`'s existing rich report
   types satisfy "quiero todo el proceso, no un simple correcto/incorrecto"
   for free — the daemon already builds that detail to answer the IPC call;
   this just also keeps a copy.

### 3. Driver/kernel identity

Logged to `driver-kernel.jsonl`:

- Once at daemon startup, from the same identity fields `system.getInfo`
  computes (kernel, driver-installed, board, DMI) — **only if it differs**
  from the last line in the file, so a normal restart with nothing changed
  doesn't add a duplicate entry.
- Again after any `installer.apply` action that touches the driver
  (`installDriver`, `restoreDriver`, `pinFanCeiling`), since that's exactly
  "cada vez que se cambian".

### 4. Daemon lifecycle

`daemon.jsonl` is written directly from `main.rs`, not through the IPC
wrapper (there's no request to hang it off): process start (with version and
whether it's running privileged), clean shutdown, and any module that failed
to initialize (today's startup sequence already logs these via `log_warn!`;
this adds a structured copy when the toggle is on).

### Shared module: `pyren_core::debuglog`

New file `daemon/crates/core/src/debuglog.rs`, sibling to `log.rs` and
following the same taste (no new dependencies, no per-category filtering
mini-language):

```rust
pub enum Category {
    DriverKernel, Performance, Lighting, Calibration,
    Cleaner, Installer, Daemon, Ipc,
}

pub fn enabled() -> bool;                 // one atomic load
pub fn set_enabled(v: bool);              // flips the atomic, persists debug.json
pub fn record(cat: Category, value: impl Serialize);  // no-op when disabled
```

`record` resolves the category to its filename, appends one JSON line
(`serde_json::to_string` + `\n`), and handles the 5 MB rotation. Every write
is wrapped so an I/O failure (disk full, permissions changed underfoot) is
swallowed and reported once via the existing `log_warn!`, never in a loop
and never by returning `Result` to a caller that would have to handle it —
logging must never be a reason the daemon refuses to do the thing it's
logging.

Every crate that needs it already depends on `core` (same reason the
`log_*!` macros are there), so `fan`, `rgb`, `installer` need no new
dependency — though in this design none of them call `debuglog` directly;
only `daemon/daemon/src/main.rs` does, keeping the wiring in the one place
that already knows about every module.

## OSD widget

`osd/` is a separate process, already running as the desktop user. It:

- Calls `debug.getStatus` once at connect and caches the result.
- Subscribes to `debug.changed` via its existing `core.nextEvent` poll
  (`osd/src/daemon.rs`) to pick up a toggle flip live.
- When enabled, appends directly to `~/.cache/pyren/depuration/widget.jsonl`
  — its own small writer (rotation logic shared by copying the same ~30
  lines from `debuglog.rs`, or by depending on `pyren_core` if it doesn't
  already — check at implementation time). Content: errors it hits talking
  to the daemon, and every mode change it draws (`hotkey.pressed`,
  `power.mode`, `fan.mode` as seen from the widget's side).

## Desktop app (Tauri)

- New Tauri command `debug_log(category: string, entry: Value)` in
  `app/src-tauri/src`, called from the frontend for:
  - uncaught JS/Svelte errors (a top-level handler already exists or is
    trivial to add),
  - a short, explicit list of "interesting" user actions (opening the driver
    wizard, toggling a safety setting, starting a calibration from the UI) —
    not every click. The list is chosen during implementation from what's
    already there; it should stay short enough to review in one sitting.
- Writes to `~/.cache/pyren/depuration/frontend.jsonl`, honoring the same
  enabled flag (`app/src/lib/api` gets a `debug.ts` mirroring `admin.ts`'s
  shape: `getStatus()`, `setEnabled()`).

## Settings UI

New section in `app/src/routes/settings/+page.svelte`, "Registros de
depuración":

- One toggle, wired to `debug.setEnabled` / reflecting `debug.getStatus`.
- Explanatory text: what it records (the list above, in plain language) and
  where (`daemonDir`, plus the user's own dir when they differ).
- "Abrir carpeta de registros" button, opening the user's own
  `~/.cache/pyren/depuration` in the file manager; if `daemonDir` differs and
  `daemonDirWritable` is true, a second link for that path.
- No export/zip button in this iteration (see Non-goals).

i18n: new keys under a `debugLogs` (or similar) namespace in both
`app/src/lib/i18n/locales/en.json` and `es.json`, following the existing
`Msg`/catalog pattern already used everywhere else in the app.

## Error handling summary

- Every `debuglog::record` call is infallible from the caller's perspective;
  I/O failure is caught, logged once via `log_warn!`, and otherwise ignored.
- A `debug.setEnabled(true)` where the target directory can't be created
  still persists the setting (the user asked for it) but `getStatus` reports
  `daemonDirWritable: false` so the UI can say so rather than silently
  producing no files.
- Rotation failure (can't rename `.jsonl` to `.jsonl.1`, e.g. permissions
  changed after directory creation) falls back to truncating the current
  file rather than growing it unboundedly or crashing.

## Testing

- `daemon/crates/core/src/debuglog.rs`: unit tests in the style already in
  `log.rs` / `config/lib.rs` — disabled means literally no file is created;
  enabled writes valid JSON lines; rotation triggers at the size cap and
  produces exactly one `.1` backup; a write error doesn't panic.
- `daemon/daemon/tests/`: an integration test (alongside the existing
  `energy_profiles.rs`) that starts a daemon with the toggle on, drives
  `power.setMode`, `fan.setMode`, `rgb.*`, and `fan.calibrate` (or the
  cheapest capability that exercises the calibration path in test mode), and
  asserts the right files got the right lines.
- Frontend: a component/store test that the Settings toggle round-trips
  through `debug.getStatus`/`setEnabled` and that the UI shows the
  not-writable state when the daemon reports it.

## Files touched (implementation-time reference)

- `daemon/crates/core/src/debuglog.rs` (new)
- `daemon/crates/core/src/lib.rs` (register module; one-line call in
  `Registry::dispatch`)
- `daemon/daemon/src/main.rs` (one-line `events.subscribe(...)` addition,
  lifecycle logging calls, `debug` module registration)
- `daemon/crates/config/src/lib.rs` — no change expected; reused as-is
- `docs/01-ipc-protocol.md` — document the new `debug` module and
  `debug.changed` topic
- `osd/src/daemon.rs`, `osd/src/main.rs` (or wherever mode changes are drawn)
- `app/src-tauri/src/lib.rs` (or an `admin.rs`-adjacent new file) —
  `debug_log` command
- `app/src/lib/api/debug.ts` (new, mirrors `admin.ts`)
- `app/src/routes/settings/+page.svelte`
- `app/src/lib/i18n/locales/en.json`, `es.json`
