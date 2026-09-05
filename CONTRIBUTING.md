# Contributing to Pyren

Pyren is three programs — a privileged Rust daemon (`daemon/`), an
unprivileged Tauri + SvelteKit app (`app/`), and the OSD widget (`osd/`) —
plus the patched kernel driver (`driver/`, never edited here). The design
and the reasoning behind the split are in [`docs/`](docs/); the work queue
and hard-won facts are in [`dev/`](dev/). This file covers the mechanics:
building, verifying, and adding a translation.

The code and its comments are in **English** to match the rest of the
repository, even though the project's own conversations are in Spanish.

## Building and running

Full prerequisites (Bun, WebKitGTK, GTK 4 + layer-shell) and the Wayland
workaround are in [`docs/02-development.md`](docs/02-development.md). Once
they are installed:

```sh
cd app
bun install
bun run dev:all      # builds daemon + widget, restarts them, runs the app
bun run dev:deps     # the same without the app, for a `tauri dev` already up
```

`bun run dev:all` is [`tools/dev-all.sh`](tools/dev-all.sh). It exists
because `bun run tauri dev` only rebuilds the frontend and `app/src-tauri`
— the daemon and the widget are separate Cargo workspaces (on purpose, so
the daemon keeps building on a machine with no GUI libraries), and the
daemon is a systemd service started from a fixed path, not a child of the
app. Change the daemon without rebuilding and restarting it and the *old*
daemon keeps answering, silently. `dev-all.sh` builds all three and
restarts what needs restarting; it asks for `sudo` once, only to restart
the daemon's service.

To run the pieces one at a time instead:

```sh
cd daemon && cargo run -p pyren-daemon      # terminal 1
cd app    && bun run tauri dev              # terminal 2
cd daemon && cargo run -q -p pyren-ctl -- status   # what the daemon sees
```

Reaching a daemon running as **root** means being in its group:

```sh
sudo groupadd -f pyren && sudo usermod -aG pyren "$USER"
# then log out and back in, or `newgrp pyren` for one shell
```

## Verifying a change

Run the same checks CI runs (`.github/workflows/ci.yml`), before pushing:

```sh
cd daemon    && cargo test --workspace && cargo clippy --all-targets -- -D warnings
cd app       && bun run check && bun run build
cd app/src-tauri && cargo check --all-targets
cd osd       && cargo test && cargo clippy --all-targets -- -D warnings
sh -n tools/pyren-check.sh && sh -n tools/power-soak.sh
```

CI splits these into separate jobs so a red badge says which half broke.
Running them locally first is faster than waiting for the badge.

Hardware-touching behaviour is checked by suites that need a real machine
and are **not** in CI:

```sh
tools/power-soak.sh          # power profiles / limits, against real firmware
tools/pyren-check.sh         # dependency-free fan self-test
```

### `TEST.md`

[`TEST.md`](TEST.md) is the honest record of what has actually been
tested and verified on real hardware, feature by feature — what works
(✅), what is only partly proven (⚠️), and what does not exist yet (❌),
with the exact command or measurement behind each claim. It is not
aspirational: every number in it comes from a suite you can run yourself,
and "verified on hardware" means someone watched it happen on this
laptop.

**If your change alters what works, update `TEST.md` in the same PR.**
Keep it truthful — an untested path stays ⚠️ or ❌ until it has actually
been exercised, and "the decision logic has tests" is not the same claim
as "the effect was seen on hardware". When in doubt, under-claim.

## Adding a translation (i18n)

There are two halves: the app's own strings, and the sentences the daemon
sends.

### App strings

The runtime is [`app/src/lib/i18n/index.svelte.ts`](app/src/lib/i18n/index.svelte.ts):
a three-tier fallback (main language → user fallback → app default `en`),
resolving keys against bundled JSON. A key missing from every catalog
renders as the key itself, so untranslated strings are visible in the UI
rather than blank.

To add a language:

1. Copy [`app/src/lib/i18n/locales/en.json`](app/src/lib/i18n/locales/en.json)
   to `<iso>.json` in the same directory — e.g. `fr.json`, `de.json`,
   `pt.json`. Use the base ISO 639-1 code, no region suffix.
2. Translate the string values. Leave the keys and the `{placeholder}`
   tokens untouched — `{name}`, `{count}` and the like are interpolated at
   runtime.
3. That's it. The build-time glob picks the file up, and it appears in
   **Settings → Language** by itself — there is no language table to edit.
   The display name comes from `Intl.DisplayNames`.

`en.json` is authoritative and must stay complete; other locales may be
partial (missing keys fall back). When you add or rename a key in the app,
add it to `en.json` (and ideally `es.json`) in the same change.

Check your file: `cd app && bun run check && bun run build`, then switch to
the new language in Settings and click through the pages.

### Daemon strings

The daemon's user-facing sentences travel as `pyren_core::Msg { key,
params, text }` (built with the `msg!` macro). The app renders them with
`tm()` / `errorText()`: it looks up `key` (+ `params`) in the same
`locales/*.json`, and falls back to the English `text` the daemon shipped.
The full contract is in
[`docs/01-ipc-protocol.md`](docs/01-ipc-protocol.md) §"Translatable
messages (`Msg`)".

So translating a daemon message is also just editing the locale JSON —
its keys live under sections like `rgb.dialect.*`, `keymap.*`,
`overclock.*`. If you add a new `msg!` call in a daemon crate, add its key
to `en.json` so there is something to translate; until then `tm()` falls
back to `text` and the string silently stays English. It is worth
periodically diffing each crate's `msg!` key namespaces against the
top-level keys in the locale files — a whole missing crate section is easy
to miss because every individual render still "works".

One known limit: where one `Msg` embeds a fragment produced by another,
the inner fragment is passed as pre-rendered English text and is **not**
re-translated (except joined lists, which `Msg::join` splits so each part
is translated). See `docs/01-ipc-protocol.md` for the details.

## Pull requests

- Branch off `main`; keep the change focused.
- Run the verification checks above.
- Update `TEST.md` if behaviour changed, and `dev/TODO.md` /
  `dev/FINDINGS.md` if you closed or discovered something there.
- If something you learned cost real effort to establish, write it into
  `dev/FINDINGS.md` so the next person doesn't re-derive it.
