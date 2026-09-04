# Design plan

Pyren is a Tauri clone of HP's OMEN Gaming Hub for Linux, built as a
**host daemon + pluggable hardware modules**, so features can be ported in
from different source repos (this project starts with fan control, ported
from `omen-fan-control`; lighting from `omen-rgb-linux` is the planned
second module) without those repos' code ever needing to touch the app
shell.

## Why a daemon + modules, not a monolith

Tauri's WebKit-based frontend process should not run as root, but nearly
everything a hub-style app needs to do (write fan PWM, drive RGB, manage a
systemd service, install a kernel module) does need root. Splitting into a
privileged daemon and an unprivileged app makes that boundary explicit and
auditable instead of scattered `pkexec` calls throughout UI code.

Modules are Rust crates implementing one shared trait (`pyren_core::Module`,
see `daemon/crates/core/src/lib.rs`), statically linked into a single
`pyren-daemon` binary — not runtime-loaded plugins (Rust's dynamic
plugin loading has real ABI stability costs that aren't worth it yet).
"Modular" here means modular *in source layout and responsibility*, so a
module ported from another repo lives in its own crate and can be
developed/tested in isolation, not that modules can be swapped without a
rebuild.

## Process/privilege layout

```
┌─────────────────────────┐
│   pyren-daemon (root)  │  systemd service
│   ┌────────┐ ┌───────┐ ┌─────┐ │
│   │ system │ │ power │ │ fan │ │  ...one crate per hardware surface
│   └────────┘ └───────┘ └─────┘ │
└────────────▲─────────────┘
             │ Unix domain socket, JSON-RPC-ish
             │ (see docs/01-ipc-protocol.md)
┌────────────┴─────────────┐
│   Tauri app (user)        │
│   src-tauri: socket client, exposes #[tauri::command]s
│   frontend: SvelteKit UI, calls invoke("fan_get_status"), etc.
└────────────────────────────┘
```

- The socket is the trust boundary, and it is enforced the only way the
  kernel will enforce it for us: file permissions. The daemon binds it
  `0660` to the `pyren` group, so being a member of that group is what
  it means to be allowed to control this machine — see
  `daemon/crates/core/src/socket.rs` and the Transport section of
  `01-ipc-protocol.md`. Day-to-day calls don't re-prompt for a password —
  the daemon simply is root, always, and the app process never
  is. A `pkexec`-driven flow only appears in the one-time installer
  (installing the patched kernel driver, installing `pyren-daemon` as a
  systemd service), reusing the approach documented in
  `docs/03-installation.md` of the `omen-fan-control` project (that
  checkout is not in this repository; `dev/README.md` says where it is).
- Every module implementation should stay read/write-capability-aware:
  read-only status calls (e.g. `fan.getStatus`) don't need root and are
  safe to leave working even before the daemon is installed as a
  privileged service — useful for local development, see
  `daemon/daemon/src/main.rs`'s socket path fallback.

## Repo layout

```
pyren-linux/
├── daemon/                 Cargo workspace, becomes the root systemd service
│   ├── daemon/              bin crate: loads modules, runs the IPC server
│   ├── ctl/                 pyren-ctl: shell client for a running daemon
│   ├── check/               pyren-check: standalone fan self-test
│   └── crates/
│       ├── core/            Module trait, Registry, wire types, socket server + client
│       ├── system/          machine identity + generic Linux monitoring
│       ├── power/           power modes + background auto-switch supervisor
│       ├── installer/       driver + service installer (inspect/plan/apply)
│       └── fan/              fan module (ported from omen-fan-control)
├── app/                     Tauri app (SvelteKit frontend + src-tauri shell)
│   └── src/routes/           UI, organized by module once more than one exists
└── docs/                    this file + the IPC protocol spec
```

`daemon/` and `app/src-tauri` are deliberately **separate Cargo workspaces**
— they build and ship as independent binaries (one installed as a systemd
service, one as a desktop app bundle), so keeping them structurally
separate matches how they're actually deployed.

## Config

Implemented in `daemon/crates/config` (`pyren-config`). Each module owns
one namespace, written as a single JSON file:

```
/etc/pyren/power.json       system config, written by the root daemon
~/.config/pyren/app.json    the desktop app's own preferences
~/.config/pyren/ui.json     UI state (fan curve, lighting, GPU mode)
```

The daemon falls back to the per-user directory when `/etc/pyren` isn't
writable, which is what makes `cargo run` usable unprivileged. Writability
is tested by actually creating the directory rather than by checking for
root, since being root and the path being writable are different questions
(read-only `/etc`, containers, immutable distros).

The `fan` module now has `fan.json` too (mode, manual speed, curve,
smoothing window, `restoreModeOnStart`). The original Python project's
persistent/volatile split — a second copy under `/run` for settings that
should not survive a reboot — was **not** carried over: it exists there to
let a shutdown hook hand state to the next boot, and nothing here has
needed that. The fan cleaner has since landed and still did not need it:
its state is a running cycle rather than a setting, and a cycle that does
not survive its own daemon is a cycle that must not survive a reboot
either — the daemon ends one it finds still running instead of resuming
it.

A `core.json` for cross-cutting settings (enabled modules, log level) still
doesn't exist, because nothing has needed one yet.

## Roadmap

1. ~~Daemon skeleton + IPC socket + Tauri shell round-trip~~ — done: `fan.getStatus` and `core.capabilities` work end-to-end.
2. ~~Port the rest of the `fan` module~~ — done, the fan cleaner included:
   config persistence, the `setMode`/`setCurve` write path, the hysteresis
   loop (a background thread inside the daemon, replacing the Python
   `serve` loop), and calibration — `fan.calibrate` runs the fans at max,
   watches them settle and puts back the mode it found, which is what gives
   the hysteresis an RPM ceiling to compare against instead of PWM values.
   A run that does not move the fans stores nothing, because a machine's
   idle speed recorded as its ceiling is worse than no calibration at all.
   What a machine can actually do is reported as `capabilities`, because
   `auto`/`max` and a *speed* have different hardware requirements — see
   `01-ipc-protocol.md`.
3. ~~Fan UI in the app matching Pyren's Performance/Fans tab~~ — done,
   along with the rest of the Pyren surface (vitals, advanced tuning,
   lighting, graphics switcher, network booster, key mapping, settings,
   drivers, help). See `docs/03-frontend.md`. The UI's *write* paths call
   the daemon; the fan ones now do something, the rest still do not.
4. ~~Fan-cleaner protocol~~ (`docs/04-fan-control-logic.md` §"Fan cleaner
   protocol" in the source repo) — done: the ACPI-call sequence, both
   firmware generations, the braking and ramp-down steps, and the three
   independent enforcements of the cycle timeout. See
   `01-ipc-protocol.md` §"The fan cleaner" for the wire shape and for what
   remains untested (the firmware's own answer).
5. Second module (RGB, from `omen-rgb-linux`) to prove the module boundary
   generalizes to a differently-shaped hardware surface — reviewed, not
   started. Which of the two unrelated paths to port **is now settled**:
   the laptop has no `0d62` USB device, so the 4-zone ACPI lightbar is the
   only candidate. See `docs/04-rgb-porting-review.md`.
6. ~~Privileged installer flow (kernel driver + daemon systemd unit)~~ —
   ported as the `installer` module (inspect/plan/apply, see
   `docs/01-ipc-protocol.md`). Its *execution* path is written but has
   never been run; running it on the test laptop is now the top item in
   `dev/TODO.md`, because it is also the experiment that would decide
   whether this board can be given a real fan percentage. The GUI wizard on
   top of it is done — `DriverWizard.svelte` at the bottom of `/drivers`,
   which shows the plan's steps and their commands and keeps "apply"
   disabled until a dry run of those exact options has been read
   (`docs/03-frontend.md`).
7. ~~Power modes as real profiles~~ — done: the laptop's own firmware
   profile, the OS profile (delegated to power-profiles-daemon), and the
   package power envelope, applied as three separable parts. The envelope
   ships untouched, because per-chassis numbers are not something this
   project can invent; `pyren-ctl power tune` is how a measured one gets
   recorded.
8. ~~Overclocking — GPU offsets~~ — done as the `overclock` module, and
   the only feature in this project that leaves the envelope the machine
   shipped with, so it is the only one behind a consent of its own: an
   offset is applied as a climb in small steps and undone automatically
   unless somebody confirms it, and nothing is restored at boot without an
   explicit opt-in. **CPU limits above the firmware's own are still not
   here**: the `power` module owns those registers and re-applies them,
   clamped to stock, on every mode change, so raising them means deciding
   which module owns the envelope. See `docs/01-ipc-protocol.md`
   §"`overclock` module" and `dev/TODO.md` §3.

## Open decisions (intentionally not settled by this scaffold)

- **Monorepo vs. multi-repo**: modules currently live as crates inside this
  one repo. If a module should be independently publishable/versioned
  later, that's a bigger change (crates.io-style versioning, workspace
  `path` deps become git/registry deps) — don't assume it until it's
  actually needed.
- ~~**Config persistence mechanism**~~ — **decided**: hand-rolled JSON, like
  the Python original, in `pyren-config`. The requirements are narrow (a
  few small files, no layering, no environment interpolation) and what
  actually matters is failure behaviour, which a config framework would not
  have given us for free:

  - **Atomic writes.** Config is written by a daemon that can be killed at
    any moment. Writing in place risks a truncated file that fails to parse
    on next boot which — for a daemon that controls fans — means silently
    reverting to defaults. Saves go to a temp file, are flushed, then
    renamed over the target.
  - **A corrupt file is never overwritten silently.** It is moved to
    `<name>.json.bad` and the user is told where it went, in both the
    daemon log and the app's Settings page.
  - **Versioned files.** A file written by a *newer* build is refused
    rather than parsed optimistically and written back in the older shape —
    downgrading must not destroy settings.

  The desktop app shares this one crate by path dependency rather than
  duplicating it. The two remain separate Cargo workspaces shipping as
  separate binaries; this is one small library in common, not a merge.
