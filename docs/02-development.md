# Development: build & run

## Prerequisites

- Rust + Cargo (`daemon/` builds with no extra system deps).
- [Bun](https://bun.sh) — `curl -fsSL https://bun.sh/install | bash`, then add
  to your shell rc:
  ```sh
  export BUN_INSTALL="$HOME/.bun"
  export PATH="$BUN_INSTALL/bin:$PATH"
  ```
- WebKitGTK + friends, required to compile `app/src-tauri` (Tauri's Linux
  webview). Arch/CachyOS:
  ```sh
  sudo pacman -S --needed webkit2gtk-4.1 base-devel curl wget file openssl \
    appmenu-gtk-module gtk3 libappindicator-gtk3 librsvg
  ```
  Other distros: <https://tauri.app/start/prerequisites/#linux>.

## 1. Run the daemon

```sh
cd daemon
cargo run -p omen-hub-daemon
```

Listens on `/tmp/omen-hub-daemon.sock` by default (unprivileged dev
fallback — see `daemon/daemon/src/main.rs`). Set `OMEN_HUB_SOCKET` to
override. Leave this running in its own terminal; the app can't do
anything useful without it.

## 2. Run the app

```sh
cd app
bun install      # first time only
bun run tauri dev
```

Without Bun installed, the same works through Node (the lockfile is Bun's,
but the dependency tree is plain npm):

```sh
node node_modules/vite/bin/vite.js dev      # frontend only, in a browser
node node_modules/@tauri-apps/cli/tauri.js dev
```

Running only the Vite dev server is the fastest loop for UI work: the app
detects it isn't inside Tauri, falls back to simulated readings and renders
every page normally (see "Demo mode" in `docs/03-frontend.md`).

This starts the Vite dev server and the Tauri/Rust shell together, then
opens the app window. First build compiles ~490 crates (WebKitGTK/GTK
bindings etc.) and takes a minute or two; rebuilds after that are fast
(only `app/src-tauri` needs recompiling).

### Wayland: window opens then immediately closes

If the window flashes open and dies with something like:

```
Gdk-Message: Error 71 (Error de protocolo) dispatching to Wayland display.
```

force GTK to run over XWayland instead of native Wayland (needs an
`X11`/XWayland `$DISPLAY` to be available, which is the default on most
desktops):

```sh
GDK_BACKEND=x11 WEBKIT_DISABLE_COMPOSITING_MODE=1 bun run tauri dev
```

This is a known WebKitGTK/compositor interaction, not specific to this
project's code — if it stops being needed on your setup, drop it.

## Checking the frontend

```sh
cd app
node node_modules/svelte-check/bin/svelte-check --tsconfig ./tsconfig.json
node node_modules/vite/bin/vite.js build
```

## Sanity-checking without the GUI

The daemon's socket can be exercised directly, which is useful when
iterating on a module without waiting on a GTK rebuild:

```sh
python3 -c '
import socket, json
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect("/tmp/omen-hub-daemon.sock")
s.sendall((json.dumps({"id":1,"module":"fan","method":"getStatus"})+"\n").encode())
print(s.recv(4096).decode())
'
```

See `docs/01-ipc-protocol.md` for the full wire format.
