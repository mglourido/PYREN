<script lang="ts">
  /**
   * Advanced performance tuning: GPU overclocking.
   *
   * The one page in this app whose buttons can leave the machine running
   * outside what the firmware shipped, and the layout is shaped by that
   * rather than by the sliders:
   *
   * - **Nothing is drawn that this machine cannot do.** The ranges, and
   *   whether each knob exists at all, come from the daemon's probe - so a
   *   card with no settable offset shows the sentence saying why instead of
   *   a slider that would always fail.
   * - **The warning is the daemon's own text**, shown before anything can
   *   be applied. The app does not get to reword what somebody agreed to.
   * - **An applied change is undone unless it is confirmed.** The daemon
   *   arms that timer; this page shows the countdown and the two buttons.
   *   Closing the app mid-countdown is not a way to keep an overclock - it
   *   is the case the timer exists for.
   */
  import Icon from "$lib/components/Icon.svelte";
  import InfoTip from "$lib/components/InfoTip.svelte";
  import Segmented from "$lib/components/Segmented.svelte";
  import Slider from "$lib/components/Slider.svelte";
  import Sparkline from "$lib/components/Sparkline.svelte";
  import Toggle from "$lib/components/Toggle.svelte";
  import { daemon, errorText, type OcGpu, type OverclockState } from "$lib/api/daemon";
  import { t, tm } from "$lib/i18n/index.svelte";
  import { formatTemp } from "$lib/stores/settings.svelte";
  import { telemetry, type Series } from "$lib/stores/telemetry.svelte";
  import { onMount } from "svelte";

  let oc = $state<OverclockState | null>(null);
  let error = $state<string | null>(null);
  let busy = $state(false);
  let selectedId = $state<string | null>(null);

  /** Staged locally: an overclock applied on every slider tick would be
   *  both pointless and dangerous. Only Apply sends anything. */
  let coreOffset = $state(0);
  let memOffset = $state(0);
  let lockClocks = $state(false);
  let lockMin = $state(0);
  let lockMax = $state(0);

  /** Ticks down between refreshes so the countdown moves once a second
   *  rather than in whatever steps the polling happens to land on. */
  let secondsLeft = $state(0);

  const gpu = $derived<OcGpu | null>(oc?.gpus.find((g) => g.id === selectedId) ?? null);
  const pending = $derived(oc?.pending ?? null);
  const canOffset = $derived(Boolean(gpu?.coreOffset || gpu?.memOffset));

  const dirty = $derived.by(() => {
    if (!gpu) return false;
    const target = gpu.applied ?? gpu.confirmed;
    return (
      coreOffset !== target.coreOffsetMhz ||
      memOffset !== target.memOffsetMhz ||
      lockClocks !== Boolean(target.coreClock) ||
      (lockClocks &&
        (lockMin !== (target.coreClock?.minMhz ?? 0) ||
          lockMax !== (target.coreClock?.maxMhz ?? 0)))
    );
  });

  /** The live readings for the card being tuned, matched by name; the
   *  vitals poller already has them, so this page adds no traffic. */
  const readings = $derived(
    telemetry.gpus.find((g) => g.name === gpu?.name) ?? telemetry.gpu ?? null,
  );

  const charts: Series[][] = $derived([
    [{ label: t("advanced.temperature"), color: "#e5178c", values: telemetry.gpuTempHistory }],
    [
      {
        label: t("advanced.utilisation"),
        color: "#2fd0ff",
        values: telemetry.gpuHistories[gpu?.name ?? ""] ?? telemetry.gpuUsageHistory,
      },
    ],
  ]);

  function stage(from: OcGpu | null) {
    const target = from?.applied ?? from?.confirmed ?? null;
    coreOffset = target?.coreOffsetMhz ?? 0;
    memOffset = target?.memOffsetMhz ?? 0;
    lockClocks = Boolean(target?.coreClock);
    // A lock that is off still needs sensible handles, so the sliders open
    // on the card's own range rather than on zero.
    lockMin = target?.coreClock?.minMhz ?? from?.clockLock?.min ?? 0;
    lockMax = target?.coreClock?.maxMhz ?? from?.clockLock?.max ?? 0;
  }

  function adopt(state: OverclockState) {
    oc = state;
    error = state.error ? tm(state.error) : null;
    if (!selectedId || !state.gpus.some((g) => g.id === selectedId)) {
      selectedId = state.defaultGpu ?? state.gpus[0]?.id ?? null;
    }
    secondsLeft = state.pending?.secondsLeft ?? 0;
    stage(state.gpus.find((g) => g.id === selectedId) ?? null);
  }

  /** Every call answers with the whole state, so one handler suits them all. */
  async function run(call: () => Promise<OverclockState>) {
    busy = true;
    try {
      adopt(await call());
    } catch (e) {
      error = errorText(e);
    } finally {
      busy = false;
    }
  }

  const apply = () =>
    run(() =>
      daemon.applyOverclock({
        gpu: selectedId ?? undefined,
        coreOffsetMhz: gpu?.coreOffset ? coreOffset : undefined,
        memOffsetMhz: gpu?.memOffset ? memOffset : undefined,
        clockLock: gpu?.clockLock
          ? lockClocks
            ? { minMhz: lockMin, maxMhz: lockMax }
            : null
          : undefined,
      }),
    );

  onMount(() => {
    // A *fresh* probe rather than the state, once, on the way in. The
    // daemon's startup probe is taken when systemd starts it - at boot,
    // before anybody has logged in - and on a laptop whose GPU offsets go
    // through an X server that is what "no display" is recorded from. The
    // page is opened by somebody who is, by definition, logged in.
    void run(() => daemon.overclockProbe());

    // While something is waiting to be confirmed the countdown is the whole
    // interface, so it is re-read from the daemon rather than trusted to a
    // local clock: the daemon's timer is the one that will act.
    const tick = setInterval(() => {
      if (!oc?.pending) return;
      secondsLeft = Math.max(0, secondsLeft - 1);
      if (!busy) void run(() => daemon.overclockState());
    }, 1000);
    return () => clearInterval(tick);
  });
</script>

<div class="advanced">
  <header class="head">
    <Segmented
      value={selectedId ?? "none"}
      options={[
        ...(oc?.gpus ?? []).map((g) => ({
          value: g.id,
          label: g.name,
          disabled: !g.drivable,
        })),
        ...(oc ? [] : [{ value: "none", label: "GPU" }]),
        // Raising the CPU's package limits belongs behind this same
        // consent, and is not here yet: the power module owns those
        // registers and re-applies them, clamped to stock, on every mode
        // change. Two owners on one knob is worse than a missing tab.
        { value: "cpu", label: "CPU", disabled: true },
      ]}
      onchange={(v) => {
        selectedId = v;
        stage(oc?.gpus.find((g) => g.id === v) ?? null);
      }}
    />
  </header>

  <section class="panel">
    <h1 class="title">
      {t("advanced.gpuOverclocking")}
      <InfoTip>{oc ? t("advanced.consentText") : t("advanced.disclaimer")}</InfoTip>
    </h1>

    {#if !oc}
      <p class="muted">{error ?? t("common.loading")}</p>
    {:else if oc.unconfirmedAtStart}
      <p class="banner warn">
        <Icon name="warning" size={16} />
        {t("advanced.unconfirmedAtStart")}
      </p>
    {/if}

    {#if oc && !oc.consent.accepted}
      <!-- The gate. Nothing below it is reachable until the daemon has
           been told the warning was read. -->
      <div class="consent">
        <h2><Icon name="warning" size={18} /> {t("advanced.consentTitle")}</h2>
        <!-- The daemon serves the canonical wording (oc.consent.text) and
             owns the acceptance record; we show the user a translated copy. -->
        <p>{t("advanced.consentText")}</p>
        <div class="actions">
          <button
            class="primary"
            disabled={busy || !oc.supported}
            onclick={() => run(() => daemon.setOverclockConsent(true))}
          >
            {t("advanced.consentAccept")}
          </button>
        </div>
        {#if !oc.supported}
          <p class="muted">{tm(oc.detail)}</p>
        {/if}
      </div>
    {/if}

    {#if oc && gpu && !gpu.drivable}
      <p class="muted detail">{tm(gpu.detail)}</p>
    {/if}

    {#if oc?.consent.accepted && gpu?.drivable}
      {#if pending}
        <div class="pending">
          <span>
            <Icon name="warning" size={16} />
            {t("advanced.pending", { seconds: Math.ceil(secondsLeft) })}
          </span>
          <div class="actions">
            <button class="ghost" disabled={busy} onclick={() => run(() => daemon.cancelOverclock())}>
              {t("advanced.revertNow")}
            </button>
            <button class="primary" disabled={busy} onclick={() => run(() => daemon.confirmOverclock())}>
              {t("advanced.keep")}
            </button>
          </div>
        </div>
      {/if}

      <div class="body">
        <div class="charts">
          {#each charts as series, i (i)}
            <Sparkline {series} max={100} height={78} columns={8} />
          {/each}
        </div>

        <dl class="stats">
          <dt>{t("advanced.temperature")}</dt>
          <dd class="v pink">{formatTemp(readings?.tempC ?? null, false)} <small>°C</small></dd>
          <dt>{t("advanced.power")}</dt>
          <dd class="v cyan">{readings?.powerW?.toFixed(0) ?? "--"} <small>W</small></dd>
          <dt>{t("advanced.speed")}</dt>
          <dd class="v orange">{readings?.clockMhz?.toFixed(0) ?? "--"} <small>MHz</small></dd>
          <dt>{t("advanced.utilisation")}</dt>
          <dd class="v purple">{readings?.usagePercent?.toFixed(0) ?? "--"} <small>%</small></dd>
        </dl>

        <div class="sliders">
          {#if gpu.coreOffset}
            <div class="slider-row">
              <span class="slider-label">{t("advanced.coreOffset")}</span>
              <Slider
                value={coreOffset}
                min={gpu.coreOffset.min}
                max={gpu.coreOffset.max}
                gradient={false}
                ariaLabel={t("advanced.coreOffset")}
                onchange={(v) => (coreOffset = v)}
              />
              <span class="slider-value">{coreOffset > 0 ? "+" : ""}{coreOffset} MHz</span>
            </div>
          {/if}

          {#if gpu.memOffset}
            <div class="slider-row">
              <span class="slider-label">{t("advanced.memoryOffset")}</span>
              <Slider
                value={memOffset}
                min={gpu.memOffset.min}
                max={gpu.memOffset.max}
                step={5}
                gradient={false}
                ariaLabel={t("advanced.memoryOffset")}
                onchange={(v) => (memOffset = v)}
              />
              <span class="slider-value">{memOffset > 0 ? "+" : ""}{memOffset} MHz</span>
            </div>
          {/if}

          {#if !canOffset}
            <p class="muted detail">{tm(gpu.detail)}</p>
          {/if}

          {#if gpu.clockLock}
            <!-- Not an overclock: the card never runs a frequency it was
                 not shipped able to run. It is here because it is the knob
                 that decides how long it is willing to stay there. -->
            <div class="slider-row">
              <span class="slider-label">
                {t("advanced.clockLock")}
                <InfoTip>{t("advanced.clockLockHint")}</InfoTip>
              </span>
              <Toggle
                checked={lockClocks}
                labelOff={t("common.off")}
                labelOn={t("common.on")}
                ariaLabel={t("advanced.clockLock")}
                onchange={(v) => (lockClocks = v)}
              />
            </div>
            {#if lockClocks}
              <div class="slider-row">
                <span class="slider-label">{t("advanced.clockFloor")}</span>
                <Slider
                  value={lockMin}
                  min={gpu.clockLock.min}
                  max={gpu.clockLock.max}
                  step={15}
                  gradient={false}
                  ariaLabel={t("advanced.clockFloor")}
                  onchange={(v) => {
                    lockMin = v;
                    if (lockMax < v) lockMax = v;
                  }}
                />
                <span class="slider-value">{lockMin} MHz</span>
              </div>
              <div class="slider-row">
                <span class="slider-label">{t("advanced.clockCeiling")}</span>
                <Slider
                  value={lockMax}
                  min={gpu.clockLock.min}
                  max={gpu.clockLock.max}
                  step={15}
                  gradient={false}
                  ariaLabel={t("advanced.clockCeiling")}
                  onchange={(v) => {
                    lockMax = v;
                    if (lockMin > v) lockMin = v;
                  }}
                />
                <span class="slider-value">{lockMax} MHz</span>
              </div>
            {/if}
          {/if}

          <p class="warn">
            <Icon name="warning" size={15} />
            {t("advanced.holdHint", { seconds: oc.holdSecs })}
          </p>

          <div class="actions">
            <button class="ghost" disabled={!dirty || busy} onclick={() => stage(gpu)}>
              {t("common.cancel")}
            </button>
            <button class="primary" disabled={!dirty || busy || Boolean(pending)} onclick={apply}>
              {t("common.apply")}
            </button>
          </div>

          <div class="footer">
            <label class="restore">
              <Toggle
                checked={oc.restoreOnStart}
                disabled={busy}
                ariaLabel={t("advanced.restoreOnStart")}
                onchange={(v) => run(() => daemon.setOverclockRestoreOnStart(v))}
              />
              <span>
                {t("advanced.restoreOnStart")}
                <InfoTip align="right">{t("advanced.restoreOnStartHint")}</InfoTip>
              </span>
            </label>
            <button
              class="ghost"
              disabled={busy}
              onclick={() => run(() => daemon.resetOverclock(selectedId ?? undefined))}
            >
              {t("advanced.backToStock")}
            </button>
          </div>
        </div>
      </div>
    {/if}

    {#if oc?.note}
      <p class="muted note">{tm(oc.note)}</p>
    {/if}
    {#if error}
      <p class="banner error"><Icon name="warning" size={16} /> {error}</p>
    {/if}
  </section>
</div>

<style>
  .advanced {
    padding: 0 0 40px;
  }

  .head {
    padding: 14px 26px;
    background: #1f1f23;
    border-bottom: 1px solid var(--line-soft);
  }

  .panel {
    margin: 20px 26px;
    background: #131316;
    border: 1px solid var(--line-soft);
    border-radius: var(--radius);
    padding: 20px 24px 26px;
  }

  .title {
    display: flex;
    align-items: center;
    gap: 10px;
    font-size: 20px;
    font-weight: 400;
    margin-bottom: 20px;
  }

  .muted {
    color: var(--text-dim);
    font-size: 13px;
    line-height: 1.5;
    margin: 0 0 14px;
  }

  .detail {
    max-width: 62ch;
  }

  .note {
    margin-top: 18px;
  }

  .banner {
    display: flex;
    align-items: center;
    gap: 8px;
    margin: 0 0 18px;
    padding: 10px 12px;
    border-radius: 4px;
    font-size: 13px;
    line-height: 1.4;
  }

  .banner.warn {
    background: #2a1f0c;
    color: var(--warn);
  }

  .banner.error {
    margin: 18px 0 0;
    background: #2a1114;
    color: #ff6b81;
  }

  /* The consent gate. Deliberately the widest thing on the page: it is
     the one paragraph on this page that has to be read. */
  .consent {
    max-width: 72ch;
    padding: 18px 20px;
    border: 1px solid var(--warn);
    border-radius: 4px;
    background: #1c1710;
  }

  .consent h2 {
    display: flex;
    align-items: center;
    gap: 8px;
    margin: 0 0 10px;
    font-size: 16px;
    font-weight: 600;
    color: var(--warn);
  }

  .consent p {
    margin: 0 0 16px;
    font-size: 13px;
    line-height: 1.6;
  }

  .pending {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 20px;
    flex-wrap: wrap;
    margin-bottom: 20px;
    padding: 12px 16px;
    border: 1px solid var(--warn);
    border-radius: 4px;
    background: #1c1710;
    font-size: 13px;
  }

  .pending span {
    display: flex;
    align-items: center;
    gap: 8px;
    color: var(--warn);
  }

  .body {
    display: grid;
    grid-template-columns: minmax(280px, 420px) auto minmax(320px, 1fr);
    gap: 40px;
    align-items: start;
  }

  .charts {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .stats {
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .stats dt {
    color: var(--text-dim);
    font-size: 14px;
  }

  .stats dd {
    margin: 0 0 14px;
    font-size: 24px;
    font-weight: 700;
  }

  .stats small {
    font-size: 15px;
    font-weight: 600;
  }

  .pink {
    color: #ff2f6d;
  }
  .cyan {
    color: #2fd0ff;
  }
  .orange {
    color: #ff8a00;
  }
  .purple {
    color: #b14cff;
  }

  .sliders {
    display: flex;
    flex-direction: column;
    gap: 20px;
  }

  .slider-row {
    display: grid;
    grid-template-columns: 1fr;
    gap: 6px;
  }

  .slider-label {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 14px;
  }

  .slider-value {
    justify-self: end;
    font-size: 13px;
    font-weight: 700;
  }

  .warn {
    display: flex;
    gap: 8px;
    margin: 0;
    color: var(--warn);
    font-size: 12px;
    line-height: 1.4;
  }

  .actions {
    display: flex;
    gap: 10px;
  }

  .footer {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
    flex-wrap: wrap;
    padding-top: 16px;
    border-top: 1px solid var(--line-soft);
  }

  .restore {
    display: flex;
    align-items: center;
    gap: 10px;
    font-size: 13px;
    color: var(--text-dim);
  }

  .ghost,
  .primary {
    padding: 8px 22px;
    border-radius: 2px;
    font-size: 13px;
    letter-spacing: 0.05em;
    text-transform: uppercase;
  }

  .ghost {
    border: 1px solid var(--line);
    background: transparent;
    color: var(--text-dim);
  }

  .primary {
    border: none;
    background: #f2f2f4;
    color: #17171a;
    font-weight: 600;
  }

  .ghost:disabled,
  .primary:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
</style>
