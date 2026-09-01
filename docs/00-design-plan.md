# Design plan

Omen Hub is a Tauri clone of HP's OMEN Gaming Hub for Linux, built as a
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

Modules are Rust crates implementing one shared trait (`omen_hub_core::Module`,
see `daemon/crates/core/src/lib.rs`), statically linked into a single
`omen-hub-daemon` binary — not runtime-loaded plugins (Rust's dynamic
plugin loading has real ABI stability costs that aren't worth it yet).
"Modular" here means modular *in source layout and responsibility*, so a
module ported from another repo lives in its own crate and can be
developed/tested in isolation, not that modules can be swapped without a
rebuild.

## Process/privilege layout

```
┌─────────────────────────┐
│   omen-hub-daemon (root)  │  systemd service
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

- The socket is the trust boundary. Day-to-day calls don't re-prompt for a
  password — the daemon simply is root, always, and the app process never
  is. A `pkexec`-driven flow only appears in the one-time installer
  (installing the patched kernel driver, installing `omen-hub-daemon` as a
  systemd service), reusing the approach documented in
  `../omen-fan-control-main/docs/03-installation.md`.
- Every module implementation should stay read/write-capability-aware:
  read-only status calls (e.g. `fan.getStatus`) don't need root and are
  safe to leave working even before the daemon is installed as a
  privileged service — useful for local development, see
  `daemon/daemon/src/main.rs`'s socket path fallback.

## Repo layout

```
omen-hub-linux/
├── daemon/                 Cargo workspace, becomes the root systemd service
│   ├── daemon/              bin crate: loads modules, runs the IPC server
│   └── crates/
│       ├── core/            Module trait, Registry, wire types, socket server
│       ├── system/          machine identity + generic Linux monitoring
│       ├── power/           power modes + background auto-switch supervisor
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

Each daemon module should own its own config file/namespace under
`/etc/omen-hub/<module>.json` once persistence is needed (not yet wired up
for the `fan` module — current state is read-only, in-memory path
discovery only). A `core.json` would hold cross-cutting settings (enabled
modules, log level). The persistent/volatile config split from the
original Python project (see
`../omen-fan-control-main/docs/04-fan-control-logic.md`) is worth keeping
specifically for the `fan` module once writes/curve/shutdown-hook land —
other modules likely won't need it.

## Roadmap

1. ~~Daemon skeleton + IPC socket + Tauri shell round-trip~~ — done: `fan.getStatus` and `core.capabilities` work end-to-end.
2. Port the rest of the `fan` module: config persistence, `setMode`/curve write path, calibration, hysteresis loop (as a background task inside the daemon, replacing the Python `serve` loop).
3. ~~Fan UI in the app matching OMEN Hub's Performance/Fans tab~~ — done,
   along with the rest of the OMEN Hub surface (vitals, advanced tuning,
   lighting, graphics switcher, network booster, key mapping, settings,
   drivers, help). See `docs/03-frontend.md`. The UI's *write* paths call
   the daemon and currently get "not implemented" back, which is the next
   thing to close.
4. Fan-cleaner protocol (`docs/04-fan-control-logic.md` §"Fan cleaner protocol" in the source repo) — the ACPI-call sequence, once basic curve control is solid.
5. Second module (RGB, from `omen-rgb-linux`) to prove the module boundary generalizes to a differently-shaped hardware surface.
6. Privileged installer flow (kernel driver + daemon systemd unit) as a GUI wizard, reusing the DKMS/hooks logic documented in the source repo.

## Open decisions (intentionally not settled by this scaffold)

- **Monorepo vs. multi-repo**: modules currently live as crates inside this
  one repo. If a module should be independently publishable/versioned
  later, that's a bigger change (crates.io-style versioning, workspace
  `path` deps become git/registry deps) — don't assume it until it's
  actually needed.
- **Config persistence mechanism**: hand-rolled JSON like the Python
  original, vs. a Rust config crate — not decided; the `fan` module has no
  writable config yet so this hasn't been forced.
