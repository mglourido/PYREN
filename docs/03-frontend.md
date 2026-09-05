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
    │                        cleaning, lighting, graphics, network, keys)
    ├── settings/  drivers/  help/
```

## State model

Three stores, deliberately separate:

- **`telemetry`** — what the machine reports. One poller for the whole app,
  started once in the root layout so history graphs stay continuous across
  navigation. Ref-counted `start()`/`stop()`.
- **`hardware`** — what the *user asked for* (power mode, fan mode, curve,
  power limits, GPU MUX mode). All of those write to the daemon —
  `power`, `fan` and `gpu` respectively; `setGpuMode` sets the choice
  locally first, then calls the `gpu` module and surfaces a refusal
  rather than pretending the write landed. Two features that could have
  lived here do not: the lighting page reads and writes the `rgb` module
  directly, and the overclock page (`/system/advanced`) the `overclock`
  module, each keeping its own state so there is no second copy of the
  zone colours or the pending-offset countdown to hold in step.
  `syncFromDaemon()` seeds the store from the machine's real state on
  startup, so the UI opens showing the mode the machine is actually in.

  Two things about the daemon-backed half are worth knowing before adding
  to it:

  - **The daemon is the authority, not the store.** A write returns the new
    state and the store adopts it, and `observeFan()` does the same on every
    telemetry poll. A machine that refuses the mode the UI is showing
    corrects the UI, rather than leaving a button lit that nothing is
    honouring.
  - **The mode can move without the app.** `watchDaemon()`, started once
    in the root layout, subscribes to the daemon's event stream through the
    Tauri shell (`core.nextEvent`, forwarded to the webview as
    `daemon-event`). A `power.mode` event re-reads the state, so the page
    follows the laptop's performance key, the on-screen display,
    `pyren-ctl` and the daemon's own supervisor. Polling for this would
    cost the same round trips whether or not anything happened, and would
    still be up to one interval late.

    It is a **no-op in a browser tab**: the dev bridge is request/response
    only, so `vite dev` shows correct data on every poll and simply does
    not react to a key press.
  - **Gate on capabilities, never on the mode.** `hardware.fan.capabilities`
    says whether this driver can be told a *speed* at all (board 8D2F can
    switch auto/max and nothing else), and `hardware.power.limits.available`
    the same for power limits. Pages hide what the machine cannot do instead
    of discovering it from a failed write.
- **`settings`** — app preferences (language, units, poll interval,
  dismissed notices).

`settings` and `hardware` persist to `~/.config/pyren/app.json` and
`ui.json`, written by the Tauri shell through the same `pyren-config`
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

When the daemon can't be reached, the telemetry store flags itself `demo`,
synthesises a plausible signal, and the layout shows a dismissible notice.
Pages then render their real layout with fake numbers instead of a wall of
`--`. It can be turned off in Settings.

A demo has to fill **every** panel it stands in for. Simulating only the
scalar gauges and leaving storage, processes and the GPU list empty is
worse than not simulating at all: half the page reads as plausible data and
the other half as an unfinished feature, with nothing to say which is
which. The synthetic signal therefore includes disks, processes, per-core
usage, clocks, temperatures and a *hybrid* GPU pair, because that is the
shape a real machine produces.

Browser development no longer implies demo mode: `vite dev` carries a
bridge to the daemon's socket (`app/dev-daemon-bridge.js`), so a browser
tab shows real readings whenever the daemon is running. Demo mode is now
what you get when there genuinely is no daemon.

## Admin mode

`lib/api/admin.ts` and the Permissions panel on `/drivers` answer a
question the rest of the app cannot: an unreachable daemon and unsupported
hardware look identical from the UI — both are a wall of demo numbers.

Every check runs in the Tauri shell, unprivileged, and needs no daemon,
which matters because "the daemon is unreachable" is one of the states
being diagnosed. Fixes run under `pkexec` from a **closed set** of actions
(`Grant` in `src-tauri/src/admin.rs`); no command string ever travels from
the webview.

Installing the systemd unit is one of them, and it is how the app grants
the daemon root: the unit runs it as root, which is what makes the
privileged readings (integrated-GPU utilisation) available, and it persists
across reboots. It cannot go through the daemon's IPC — the unit is what
makes the daemon privileged in the first place — so the app runs the daemon
binary itself with `--install-service`, driving the same installer code the
IPC method does. This is deliberately not a toggle in Settings: it is a
one-time system change that needs authentication, not a preference the app
owns and can flip back. The panel keeps group *database* membership separate from what
the login session actually carries — `usermod` takes effect immediately in
one and not at all in the other until the user logs back in, and treating
them as one thing is how you get a checklist that says "fixed" while
nothing works.

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
- `@tauri-apps/api` is imported only under `lib/api/` (`daemon.ts`,
  `config.ts`, `admin.ts`), never from a component, so the app also runs in
  a plain browser. Each of those has a defined behaviour outside Tauri:
  `daemon.ts` goes through the dev-server bridge, the other two report
  themselves unavailable.
- Icons are inline SVG paths in `Icon.svelte` — no icon package.

## The driver wizard

`lib/components/DriverWizard.svelte`, at the bottom of `/drivers`, is the
front end of the installer module's **inspect → plan → apply** split
(`docs/01-ipc-protocol.md`). It is a wizard rather than a button because
what it runs unloads a kernel module, replaces a file under `/lib/modules`
and regenerates the initramfs — the steps are shown, with the exact command
each one would run, before any of them do.

Installing is offered as a **choice of two modes**, not one path with an
escape hatch:

- **Automatic** runs `installer.autodetect` — the board id from DMI, which
  of the driver's tables it belongs in from the driver's own source, the
  fan ceilings from the last calibration — and then dry-runs the plan those
  answers produce. It removes the *typing*, not the reading: what was
  detected, the reasoning behind each answer (`notes`, rendered with
  `tm()`), and the steps all appear before anything is authorised.
- **Manual** is the same plan built from values typed in, for someone who
  knows their board and disagrees with what was detected.

The mode is part of the request, so switching it throws a dry run away like
any other option, and the manual fields are **not sent while they are off
screen** — a value the user cannot see must not decide what runs. Whichever
mode is chosen, confirming is a second, separate click and stays
unreachable until a dry run of those exact options has come back.

Five more rules, all of them about not letting the UI outrun what the user
has actually read:

- **Closed by default, and the verdict comes first.** On a modern kernel
  manual fan control is upstream, so `inspect`'s `patchNeeded: false` is the
  common answer and the panel leads with it. An install button above that
  sentence would be an invitation to downgrade a working driver.
- **Apply is unreachable until a dry run of the *same* options has been
  read.** The options are serialised into a key; typing in any field throws
  the plan, the report and that key away. So what is on screen is always
  the plan that would run, never a plan for options that have since
  changed.
- **Optional steps are the user's call.** Each one in the step list carries
  a switch; required steps have no switch at all rather than a disabled
  one, since the daemon refuses to skip them anyway. Unticking one throws
  the dry run away but *keeps the plan* — the step list is where the
  switches live, so deleting it on every click would make them unusable.
  That is why the component tracks two keys: `planKey` (action, hooks,
  force — what decides the steps) clears the plan, and `optionsKey`
  (everything) disarms the apply.
- **A plan with blockers is not offered.** The daemon refuses one anyway
  (`notCapable`), so the button stays disabled and the blockers are listed
  with the command that fixes each — missing kernel headers being the
  common case.
- **The RPM ceiling is offered, not invented.** `fan.calibrate` is the only
  thing that knows this chassis's full speed; where it has run, the panel
  offers the measured number for the driver's `OMEN_CPU_MAX_RPM` patch and
  otherwise leaves the field blank, which means "keep the driver's own
  fallback".

The board-id fields are the untested-hardware path: adding a board to one
of the driver's tables also picks which EC offset it reads. Autodetect will
fill them in for an OMEN or a Victus, choosing the conservative variant of
that family and saying in a note that the choice is a choice — but on a
machine that identifies as neither it leaves them empty rather than
guessing, because the two families write different thermal-profile values.

Installing the *service* is not here — it is a `pkexec` action in the
Permissions panel above, because a unit that makes the daemon root cannot
be installed by asking the unprivileged daemon to do it.

## What every module actually drives (frontend side)

Every page below reaches a real daemon module; nothing here is wired to
local state alone any more.

- Key mapping (`/system/keys`) drives the `keymap` module: an evdev-level
  remapper opening `/dev/uinput`, with mappings stored by the daemon
  rather than the page's own state. Built and wired end to end; not yet
  run against real hardware (`dev/TODO.md`).
- GPU switching is **wired** to the `gpu` module: MUX mode read at startup
  and set through `hp-wmi`'s own `gpu_mux_mode`, no `supergfxctl`.
- Network booster is **wired**, but only for the system-wide half: `Off`
  and `Auto` drive the `network` module, which hands the default-route
  interface `cake` (or `fq_codel`) — see `docs/01-ipc-protocol.md`
  §"`network` module". The per-application priority/block table the page
  used to mock up is gone; per-process traffic control needs
  cgroups/nftables/eBPF this project does not implement (`dev/TODO.md`
  §2), so the page shows total bandwidth and says plainly that
  per-application prioritisation is not available, rather than a table
  with no daemon behind it.
- Lighting is **wired** (`system/lighting`) to the `rgb` module: zone
  colours, brightness, off, restore-on-start, a read-back button that asks
  the firmware, and a Protocol panel that lists the three lighting dialects,
  says what each one answered, and lets the user pin one instead of the
  automatic pick. What it cannot offer is effects — the ACPI protocol
  carries colours and a brightness and nothing else, so there is no
  breathing or wave, and the page says so rather than showing a switch
  that does nothing. Per-key USB keyboards are reported when attached and
  explicitly not driven.
