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
  power limits, lighting, GPU mode). Power and fan writes go to the daemon;
  lighting and GPU stay local-only until those daemon paths land.
  `syncFromDaemon()` seeds the store from the machine's real state on
  startup, so the UI opens showing the mode the machine is actually in.

  Two things about the daemon-backed half are worth knowing before adding
  to it:

  - **The daemon is the authority, not the store.** A write returns the new
    state and the store adopts it, and `observeFan()` does the same on every
    telemetry poll. A machine that refuses the mode the UI is showing
    corrects the UI, rather than leaving a button lit that nothing is
    honouring.
  - **Gate on capabilities, never on the mode.** `hardware.fan.capabilities`
    says whether this driver can be told a *speed* at all (board 8D2F can
    switch auto/max and nothing else), and `hardware.power.limits.available`
    the same for power limits. Pages hide what the machine cannot do instead
    of discovering it from a failed write.
- **`settings`** — app preferences (language, units, poll interval,
  dismissed notices).

`settings` and `hardware` persist to `~/.config/omen-hub/app.json` and
`ui.json`, written by the Tauri shell through the same `omen-hub-config`
crate the daemon uses - so app settings get the same atomic writes,
corruption recovery and version stamping.

Loading is two-stage (`lib/stores/persistence.ts`). Disk is the source of
truth, but reading it is asynchronous and the first paint needs a language
*now*; hydrating a moment later would flash English before switching. So
each store also mirrors its values into `localStorage` and reads that
synchronously for the first render, then reconciles with the file.
localStorage is a **cache, never the record**: it is only written alongside
a disk write, and the file wins on any disagreement.

Writes are debounced (300 ms) so dragging a slider doesn't produce a write
per frame, and flushed on `beforeunload` so nothing queued is lost when the
window closes. Daemon calls are debounced separately and more tightly
(200 ms, `FAN_PUSH_DEBOUNCE_MS`): a fan-curve point or a power-limit slider
is a socket round trip that re-applies settings to hardware, which is a
different cost from writing a JSON file.

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

- Per-process **GPU** usage: the column exists and shows `--`. CPU, memory
  and the disk list are real.
- The installer wizard on `/drivers`. The page runs the hardware check;
  what it cannot yet do is walk someone through `installer.plan` /
  `installer.apply`.
- Lighting, GPU switching, network booster and key mapping drive local
  state only — there is no daemon module behind any of them yet.
