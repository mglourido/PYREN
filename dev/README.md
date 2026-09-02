# dev/ — working notes

Notes for whoever picks this up next, including future me. Three files:

| file | what it holds |
|---|---|
| [`TODO.md`](TODO.md) | everything still to do, in priority order, with what blocks each item |
| [`FINDINGS.md`](FINDINGS.md) | facts that cost real effort to establish — don't re-derive them |
| this file | where things are, and how to get running |

`docs/` is the *documentation*: how the thing works, for someone using or
extending it. `dev/` is the *work queue*: what's missing and what to be
careful of. If something here becomes settled and permanent, it belongs in
`docs/` instead.

These notes are in English to match the rest of the repository, even though
the project's conversations are in Spanish.

## Where things are

```
daemon/                 Rust workspace (root daemon + the CLI checker)
├── daemon/             omen-hub-daemon: loads modules, serves the socket
├── check/              omen-hub-check: standalone fan self-test CLI
└── crates/
    ├── core/           Module trait, Registry, wire types, socket server
    ├── config/         on-disk settings (atomic writes, versioning)
    ├── system/         machine identity + generic Linux monitoring
    ├── power/          power modes + background auto-switch supervisor
    ├── fan/            fan status + the self-test (diagnostics.rs)
    └── installer/      driver/service installer (inspect → plan → apply)
app/                    Tauri app: SvelteKit frontend + src-tauri shell
tools/omen-check.sh     dependency-free shell twin of omen-hub-check
docs/                   design, IPC protocol, development, frontend, RGB review
```

## Running everything

```sh
# terminal 1
cd daemon && cargo run -p omen-hub-daemon

# terminal 2
cd app && bun run tauri dev
```

Full prerequisites and the Wayland workaround are in
[`docs/02-development.md`](../docs/02-development.md).

## What actually works today

- **Monitoring**: real, on any machine. CPU per core, memory, hwmon
  temperatures and fans, disks, network, GPU, top processes.
- **Machine identification** and a supported/untested/unsupported verdict.
- **Power modes**: platform_profile → power-profiles-daemon → EPP, plus the
  background Eco/Performance supervisor, with settings that survive a
  restart.
- **Fan self-test**: three front ends (daemon method, app page, CLI + shell
  script), kept in step by a parity test.
- **Frontend**: the whole OMEN Hub surface, bilingual, with settings on disk.

## What does not work yet

- **Setting fan speed.** `fan.setMode`/`setCurve` are unimplemented, and on
  the one HP laptop tested the kernel exposes no `pwm1` anyway — see
  `FINDINGS.md` §"Board 8D2F".
- **Lighting, GPU switching, network booster, key mapping**: the UI is
  complete and drives local state only. No daemon module behind any of them.
- **The installer's execution path** has never been run.

## Verifying a change

```sh
cd daemon && cargo test && cargo clippy --all-targets   # 15 suites, 0 warnings
cd app && bun run check && bun run build
cd app/src-tauri && cargo check
```

There is no CI, so this is manual — see `TODO.md`.
