# Telemetry polling stops when no live-data page is on screen

*2026-09-17 — `app/src/lib/stores/telemetry.svelte.ts`, `app/src/routes/+layout.svelte`.*

## The problem

The app ran one shared poller (`Telemetry`) from the root layout, started
once and never stopped for as long as the app was open. Every tick — on
every page, including Settings, Lighting, Drivers, Help — it called
`daemon.getMetrics`, which the daemon answers with a full parallel sweep:
`/proc` CPU/memory reads, a disk `statvfs` pass, a `/proc/*` process-table
walk, and — when a discrete GPU is present — an `nvidia-smi` subprocess
spawn. On a laptop, spawning that subprocess and walking every process
every 2 seconds keeps the dGPU driver and the CPU awake regardless of what
the user is looking at, which costs battery for no benefit: nobody was
reading any of those numbers unless they were on Vitals, Performance,
Advanced, Network, or the home page.

## What changed

`telemetry.svelte.ts` now exports `DETAIL_ROUTES` (the five routes that
actually render live CPU/GPU/RAM/disk/network/process fields: `/`,
`/system/vitals`, `/system/performance`, `/system/advanced`,
`/system/network`) and a pure `isDetailRoute(pathname)` check. A
`detailActive` flag on `Telemetry`, driven from one `$effect` in
`+layout.svelte`, gates the poll: `daemon.getMetrics` and history recording
are skipped entirely — not called, not queued — while the active route
isn't one of the five. `daemon.fanStatus` (cheap, no subprocess) still runs
every tick everywhere, because the sidebar's reachability indicator, the
daemon-down banner, and fan-floor notifications read it from any page.

Turning `detailActive` back on fires an immediate poll, so returning to
Vitals doesn't sit on stale numbers for the rest of the current interval.

## Why routing, not per-page subscriptions

The alternative — each live-data page registering/unregistering itself
with the poller in its own `onMount` — was considered and rejected: it
scales the same way but spreads the failure mode (forgetting to register)
across five files instead of one. A single route whitelist next to the
flag it drives is the one place a future live-data page has to remember to
touch, and it's already covered by a unit test asserting every entry in
`DETAIL_ROUTES` satisfies `isDetailRoute`.

## Caught in review

A whole-branch review after the change landed found that the new `$effect`
was declared *after* the layout's existing `onMount` (which fires the
poller's first immediate poll). Since Svelte effects run in source order,
the very first poll on cold boot into the home page ran before
`detailActive` was set, got skipped, and the follow-up poll that
`setDetailActive(true)` should have triggered was then swallowed by the
poller's own in-flight guard — so the landing page briefly showed the
store's hardcoded placeholder values as if they were live. Fixed by moving
the `$effect` above the `onMount` block, so the route is known before the
first poll fires.
