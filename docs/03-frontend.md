# Frontend (app/)

SvelteKit 5 (runes) + TypeScript, built as a static SPA (`adapter-static`,
`ssr = false`) and loaded by the Tauri webview. The visual design is a
reproduction of HP's OMEN Gaming Hub — no HP code or artwork is used; the
brand marks and header art are drawn in CSS.

## Layout

```
app/src/
├── app.html                 shell (title, favicon)
├── lib/
│   ├── api/daemon.ts        the only place that calls Tauri `invoke`
│   ├── nav.ts               the device tab list (sidebar + tab bar share it)
│   ├── version.ts           app version + GitHub update check
│   ├── i18n/
│   │   ├── index.svelte.ts  t(), locale state, 3-tier fallback
│   │   └── locales/*.json   one file per language
│   ├── stores/
│   │   ├── settings.svelte.ts    user preferences (localStorage)
│   │   ├── hardware.svelte.ts    desired hardware state + write calls
│   │   └── telemetry.svelte.ts   polled readings + history + demo fallback
│   ├── components/          Icon, Sidebar, TabBar, ModeCard, Slider, Toggle,
│   │                        Gauge, Sparkline, FanCurve, Keyboard, ...
│   └── styles/theme.css     design tokens; every colour comes from here
└── routes/
    ├── +layout.svelte       chrome + global notices, starts telemetry polling
    ├── +page.svelte         home ("gaming performance toolkit")
    ├── system/              the device tabs (vitals, performance, advanced,
    │                        lighting, graphics, network, keys)
    ├── settings/  drivers/  help/
```

## State model

Three stores, deliberately separate:

- **`telemetry`** — what the machine reports. One poller for the whole app,
  started once in the root layout so history graphs stay continuous across
  navigation. Ref-counted `start()`/`stop()`.
- **`hardware`** — what the *user asked for* (power mode, fan mode, curve,
  power limits, lighting, GPU mode). Power-mode writes go to the daemon and
  report back what was actually applied (`hardware.lastApply`); fan,
  lighting and GPU writes stay local-only until those daemon paths land.
  `syncFromDaemon()` seeds the store from the machine's real state on
  startup, so the UI opens showing the mode the machine is actually in.
- **`settings`** — app preferences (language, units, poll interval,
  dismissed notices).

`settings` and `hardware` persist to `localStorage`. Moving them to
`~/.config/omen-hub/*.json` via a Tauri command later is a change to
`load`/`save` in those two files and nothing else.

## Demo mode

When the daemon can't be reached (browser dev, daemon not started), the
telemetry store flags itself `demo`, synthesises a plausible signal, and the
layout shows a dismissible notice. Pages then render their real layout with
fake numbers instead of a wall of `--`, which is what makes UI work possible
without root or the patched driver. It can be turned off in Settings.

## i18n

`lib/i18n/index.svelte.ts` keeps the three-tier fallback (main language →
user fallback → `en`) but resolves it from JSON bundled at build time
(`import.meta.glob`), not from the filesystem: this code runs inside the
webview, where there is no Bun and no file access. A missing key renders as
the key itself, so untranslated strings are visible rather than blank.

**Adding a language**: copy `lib/i18n/locales/en.json`, translate the
values, save as `<iso>.json` in the same folder. It appears in Settings
automatically — there is no language table to update.

## Conventions

- No colour literals in components; add a token to `theme.css` instead.
- Only `lib/api/daemon.ts` imports `@tauri-apps/api`, so the app also runs
  in a plain browser (`vite dev`) for UI work.
- Icons are inline SVG paths in `Icon.svelte` — no icon package.

## Not built yet (frontend side)

- Per-process CPU/GPU/RAM and network rows (tables are laid out, data is
  placeholder) — needs daemon support.
- The privileged installer flow on `/drivers` (buttons are disabled).
- Storage/disk list is hardcoded until the daemon reports mounts.
