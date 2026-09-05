# Installing Pyren

This folder is the installer: `install.sh`, its inverse `uninstall.sh`, the
`.desktop` entry they place, and this guide.

- [Install from a release](#install-from-a-release) — for everyone
- [Install from a source checkout](#install-from-a-source-checkout)
- [What it puts where](#what-it-puts-where)
- [Uninstall](#uninstall)
- [Building a release](#building-a-release) — for maintainers
- [Publishing on GitHub](#publishing-on-github)

---

## Install from a release

Download the latest `pyren-<version>-x86_64-linux.tar.gz` from the
[Releases page](https://github.com/mglourido/PYREN/releases), then:

```sh
tar xzf pyren-*-x86_64-linux.tar.gz
cd pyren-*/

sha256sum -c SHA256SUMS       # optional: check the download (run from the
                              # directory that also holds SHA256SUMS)

sudo ./install.sh
```

`install.sh` needs root only to write under `/usr` and `/etc`; it asks with
`sudo` at each step. Run `./install.sh --dry-run` first to see every action
without doing any of it.

Then, **in your own session**:

```sh
newgrp pyren                              # or log out and back in
systemctl --user enable --now pyren-osd.service   # optional: the perf-key widget
```

The group change is what lets the app and `pyren-ctl` reach the daemon —
until your session has it, you get a permission error. `newgrp pyren`
fixes it for one shell; a real login fixes it everywhere.

Now launch **Pyren** from your application menu, or run `pyren`.
`pyren-check` prints what this laptop can be told to do; `pyren-ctl status`
shows what the running daemon has switched on.

### Requirements

- A Linux kernel with the `hp-wmi` driver. Recent kernels have manual fan
  control upstream; where a board is missing from the stock driver's
  tables, Pyren's own installer (in the app, or `pyren-ctl`) can build a
  patched module from the sources under `/usr/share/pyren/driver`.
- **WebKitGTK** (`webkit2gtk-4.1`) for the app, and **GTK 4** +
  **gtk4-layer-shell** for the widget. Most desktop installs already carry
  these. On Arch / CachyOS:
  `sudo pacman -S --needed webkit2gtk-4.1 gtk4 gtk4-layer-shell librsvg`.
- Optional: `acpi_call` for the lightbar and the fan cleaner; the NVIDIA
  driver (with `libnvidia-ml`) for GPU tuning.

### Options

```
sudo ./install.sh                 install everything (prefix /usr/local)
sudo ./install.sh --prefix /usr   install under /usr instead
sudo ./install.sh --no-service    place the files, but do not touch systemd
                                  units or the pyren group
sudo ./install.sh --driver-dir D  driver sources to D instead of
                                  /usr/share/pyren/driver
     ./install.sh --dry-run       print every action, change nothing
```

The driver sources go to `/usr/share/pyren/driver` regardless of `--prefix`
— the daemon looks for them there by name. `--driver-dir` moves them, but
then the daemon needs `PYREN_DRIVER_DIR` pointing at the new location
(`systemctl edit pyren-daemon`).

---

## Install from a source checkout

The simplest path is `tools/release.sh` — it builds every part correctly
and you install from the archive it produces.

To do it by hand, `install/install.sh` also runs from a clone once the
release binaries exist:

```sh
(cd daemon && cargo build --release)
(cd osd && cargo build --release)
(cd app && bun install && bun run tauri build --no-bundle)   # NOT plain cargo build

sudo install/install.sh
```

The app **must** go through `tauri build` (or `cargo build --release
--features custom-protocol`) — a plain `cargo build` leaves the frontend
out of the binary.

---

## What it puts where

| path | what |
|---|---|
| `<prefix>/bin/{pyren,pyren-daemon,pyren-ctl,pyren-check,pyren-osd}` | the binaries (`<prefix>` defaults to `/usr/local`) |
| `/usr/share/pyren/driver/` | the patched `hp-wmi` sources (fixed path) |
| `<prefix>/share/applications/pyren.desktop` | the app-menu entry |
| `<prefix>/share/icons/hicolor/*/apps/pyren.*` | the icon |
| `<prefix>/lib/systemd/user/pyren-osd.service` | the widget's user service |
| `/etc/systemd/system/pyren-daemon.service` | the daemon's system service — written by `pyren-daemon --install-service`, not copied, so it always names the right binary |

It also runs `groupadd -f pyren` (the group the daemon's socket is handed
to) and `usermod -aG pyren <you>`.

---

## Uninstall

```sh
sudo ./uninstall.sh              # binaries, units, driver sources, icons
sudo ./uninstall.sh --purge      # …and /etc/pyren + ~/.config/pyren (your settings)
```

The `pyren` group is left in place — removing a group that accounts still
belong to is not the installer's call, and an empty group costs nothing.

---

# Building a release

*(maintainers — the rest of this file)*

A release is one local build. There is no release CI: it is run rarely, it
needs a machine that can already build every part of the project, and a
script means the person cutting the release watches it happen.

### Prerequisites

The normal build toolchain (`docs/02-development.md`): Rust + Cargo, Bun,
`webkit2gtk-4.1`, `gtk4` + `gtk4-layer-shell`. Plus `git`, `tar`,
`sha256sum`; and `gh` for `--publish`.

A full release build compiles all three workspaces from scratch in release
mode (~490 crates for the Tauri shell alone) — **10–20 minutes** on the
first run, several GB added to `target/`. Later runs reuse the caches.

### 1 · Set the version

The version lives in five manifests, and `tools/release.sh` refuses to run
unless they agree:

```sh
tools/bump-version.sh 0.2.0      # daemon, osd, src-tauri, package.json, tauri.conf.json
tools/bump-version.sh --show     # confirm; also refreshes the three Cargo.lock files
```

### 2 · Update the changelog

Move the new version's notes out of `## [Unreleased]` in `CHANGELOG.md` and
date them. `tools/release.sh --publish` uses that section verbatim as the
GitHub release notes.

### 3 · Commit

```sh
git add -A && git commit -m "release: 0.2.0"
```

`release.sh` refuses a dirty tree (`--allow-dirty` is for throwaway builds
only).

### 4 · Build

```sh
tools/release.sh                 # CI checks, then build and pack
tools/release.sh --skip-tests    # skip the checks if CI is already green on this commit
tools/release.sh --appimage      # also produce a portable AppImage of the app
```

Phases: preflight → checks (`cargo test` + `clippy` + `svelte-check`, the
same set as `.github/workflows/ci.yml`) → build → stage → package.

Output in `dist/`:

| file | |
|---|---|
| `pyren-<version>-x86_64-linux.tar.gz` | the release — every binary, the driver sources, the systemd units, `install.sh` |
| `SHA256SUMS` | checksums |
| `pyren-<version>-x86_64.AppImage` | only with `--appimage` |

The app is built through `tauri build --no-bundle` (not a plain
`cargo build` — only the CLI enables `tauri/custom-protocol`, without which
the binary ships an empty frontend and expects a dev server). `--no-bundle`
keeps it to one self-contained ELF, so no `.deb`/`.rpm`/AppImage tooling has
to be installed. The daemon and widget are plain `cargo build --release
--locked`. Reproducible: lockfiles are committed; the release profile is
`lto = "thin"`, `codegen-units = 1`, `strip = "symbols"`.

### 5 · Verify the archive

On a **different** machine or a container/VM — `install.sh` writes under
`/usr` and `/etc` and enables a system service:

```sh
tar xzf dist/pyren-0.2.0-x86_64-linux.tar.gz
cd pyren-0.2.0
sudo ./install.sh
newgrp pyren
systemctl is-enabled pyren-daemon    # -> enabled
pyren-check                          # the compatibility verdict
pyren-ctl status                     # the daemon answers
pyren                                # the app opens
sudo ./uninstall.sh
```

---

## Publishing on GitHub

### 6 · Tag and push

```sh
git tag -a v0.2.0 -m "Pyren v0.2.0"
git push --follow-tags
```

### 7 · Create the release

```sh
tools/release.sh --skip-tests --publish
```

This creates a **draft** GitHub release `v0.2.0`, uploads
`dist/pyren-0.2.0*` and `SHA256SUMS`, and fills the notes from the
matching `CHANGELOG.md` section. Review it on the Releases page and hit
**Publish**.

By hand instead: create a release for the tag on GitHub and upload
everything in `dist/`.

### Troubleshooting

| symptom | cause |
|---|---|
| `missing build libraries: webkit2gtk-4.1` / `gtk4-layer-shell-0` | install the dev package (`docs/02-development.md`) |
| `version mismatch` in preflight | run `tools/bump-version.sh <version>` |
| `working tree is not clean` | commit, or `--allow-dirty` for a throwaway build |
| AppImage build fails on `linuxdeploy` / FUSE | `--appimage` needs FUSE on the host (Tauri fetches `linuxdeploy` itself). The tarball path needs neither — drop `--appimage` |
| a flaky socket test under `cargo test --workspace` | known (`dev/FINDINGS.md`); re-run, or `--skip-tests` on a commit CI already passed |
| `.deb` / `.rpm` wanted | not produced — Pyren ships the tarball only. `tauri build` can make them on a Debian/Fedora host if someone needs one |
