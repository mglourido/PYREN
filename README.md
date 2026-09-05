# Pyren

A Tauri-based clone of HP's OMEN Gaming Hub for Linux, built as a
privileged daemon (Rust) plus an unprivileged desktop app (Tauri +
SvelteKit), The project aims to be modular to facilitate the import of external source code and to reduce the security issues that granting privileges can cause.

To create this application, the codebase was derived from the [`omen-fan-control`](https://github.com/arfelious/omen-fan-control) repositories; specifically, the patched **Linux driver**, its Python-based installer (v2.0), and the reverse ventilation logic, while [`omen-rgb-linux`](https://github.com/arfelious/omen-rgb-linux.git) was used to create the RGB control system.

[`TEST.md`](TEST.md) is what has actually been tested and verified on
real hardware.

`pyren-ctl status` is the fastest way to see what a running daemon
thinks the machine can do.

## Layout

```
daemon/     Rust workspace: pyren-daemon + pyren-ctl + pyren-check + module crates
app/        Tauri app: SvelteKit frontend + src-tauri shell
osd/        the on-screen-display widget, its own Cargo workspace (GTK layer-shell)
driver/     the patched hp-wmi kernel module, a verbatim copy of upstream's (C, GPL-2)
docs/       design plan + IPC protocol + development + frontend + RGB review
dev/        working notes: what is left to do, and what was learned
tools/      pyren-check.sh, the dependency-free fan self-test
```