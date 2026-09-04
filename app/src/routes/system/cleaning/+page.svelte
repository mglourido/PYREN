<script lang="ts">
  /**
   * Fan cleaning: dust removal by spinning the fans backwards.
   *
   * The page is arranged around the fact that this is the one control in
   * the app that **removes cooling** for as long as it runs. So the state
   * a running cycle is in gets the whole top of the page - a countdown and
   * a stop button, nothing else competing with them - and the settings
   * below are only reachable when nothing is running.
   *
   * The other thing it has to get right is *why* it is unavailable, when
   * it is. Three unrelated reasons look identical from here - no
   * `acpi_call`, no root, no such firmware - and only the last is about
   * the hardware. The daemon tells them apart (`unreachable` vs
   * `answered`); this page shows which, rather than one grey "not
   * supported".
   */
  import Banner from "$lib/components/Banner.svelte";
  import Icon from "$lib/components/Icon.svelte";
  import InfoTip from "$lib/components/InfoTip.svelte";
  import Panel from "$lib/components/Panel.svelte";
  import Slider from "$lib/components/Slider.svelte";
  import Toggle from "$lib/components/Toggle.svelte";
  import { daemon, errorText, type FanCleanerStatus } from "$lib/api/daemon";
  import { t, tm } from "$lib/i18n/index.svelte";
  import { formatTemp } from "$lib/stores/settings.svelte";
  import { telemetry } from "$lib/stores/telemetry.svelte";
  import { onMount } from "svelte";

  /** The range the daemon clamps to; kept in step with `cleaner.rs`. */
  const SPEED = { min: 10, max: 39 };
  const SECONDS = { min: 5, max: 60 };

  /** While a cycle runs the countdown is the whole point of the page, so
   *  it is polled faster than the idle refresh. */
  const POLL_RUNNING_MS = 1000;
  const POLL_IDLE_MS = 5000;

  let status = $state<FanCleanerStatus | null>(null);
  let error = $state<string | null>(null);
  let busy = $state(false);
  let loaded = $state(false);

  /** Local edits to the two settings, pushed on release rather than on
   *  every pixel of a drag. */
  let seconds = $state(30);
  let useCustomSpeed = $state(false);
  let speed = $state(37);

  async function refresh(probeFirmware = false) {
    try {
      const next = await daemon.fanCleanerStatus(probeFirmware);
      status = next;
      error = null;
      // The daemon's stored preferences win whenever nothing is being
      // dragged here: this page is a view of them, not a second copy.
      if (!busy) {
        seconds = next.durationSecs;
        useCustomSpeed = next.configuredSpeed !== null;
        if (next.configuredSpeed !== null) speed = next.configuredSpeed;
      }
    } catch (e) {
      error = errorText(e);
    } finally {
      loaded = true;
    }
  }

  // The first read asks the firmware; the polling ones use the daemon's
  // cached answer, because two ACPI calls a second on the same file the
  // lightbar writes through is not a poll, it is contention.
  onMount(() => {
    void refresh(true);
    let timer: ReturnType<typeof setTimeout>;
    const tick = () => {
      timer = setTimeout(async () => {
        await refresh(false);
        tick();
      }, status?.running || status?.transitioning ? POLL_RUNNING_MS : POLL_IDLE_MS);
    };
    tick();
    return () => clearTimeout(timer);
  });

  async function start() {
    busy = true;
    error = null;
    try {
      status = await daemon.startFanCleaning({
        seconds,
        speed: useCustomSpeed ? speed : undefined,
      });
    } catch (e) {
      error = errorText(e);
    } finally {
      busy = false;
      void refresh(false);
    }
  }

  async function stop() {
    busy = true;
    error = null;
    try {
      status = await daemon.stopFanCleaning();
    } catch (e) {
      error = errorText(e);
    } finally {
      busy = false;
      void refresh(false);
    }
  }

  async function saveSettings() {
    busy = true;
    try {
      status = await daemon.setFanCleanerConfig({
        seconds,
        speed: useCustomSpeed ? speed : null,
      });
      error = null;
    } catch (e) {
      error = errorText(e);
    } finally {
      busy = false;
    }
  }

  const running = $derived(status?.running ?? false);
  const transitioning = $derived(status?.transitioning ?? false);
  /** The fans are the cleaner's through both transitions, not only while
   *  the countdown is on screen. */
  const active = $derived(running || transitioning);

  const tooHot = $derived(
    status?.cpuTempC != null && status.cpuTempC > (status?.maxStartTempC ?? 70),
  );

  /** Only ever disabled for a reason this page has already explained
   *  somewhere the user can see. */
  const canStart = $derived(
    !!status && status.supported && !active && !busy && !tooHot && !telemetry.demo,
  );

  const progress = $derived(
    status?.secondsTotal && status.secondsRemaining !== null
      ? 1 - status.secondsRemaining / status.secondsTotal
      : 0,
  );
</script>

<div class="cleaning">
  <div class="stage">
    <h1 class="title">{t("cleaning.title")}</h1>
    <p class="lead">{t("cleaning.lead")}</p>

    <!-- What a cycle actually does to the machine, said once, at the top,
         before the button that does it. -->
    <Banner kind="warning" title={t("cleaning.warningTitle")}>
      {t("cleaning.warningBody", { limit: formatTemp(status?.maxStartTempC ?? 70) })}
    </Banner>

    {#if telemetry.demo}
      <p class="notice">{t("notices.daemonDownBody")}</p>
    {/if}

    {#if error}
      <p class="notice err">{error}</p>
    {:else if status?.error}
      <p class="notice err">{tm(status.error)}</p>
    {/if}

    <Panel>
      {#if !loaded}
        <p class="notice">{t("common.loading")}</p>
      {:else if active}
        <!-- A running cycle gets the panel to itself: a countdown, and the
             one button that ends it. -->
        <div class="cycle">
          <div class="dial" style="--progress:{progress}">
            <span class="digital count">
              {#if running && status?.secondsRemaining !== null}
                {status?.secondsRemaining}s
              {:else}
                <Icon name="refresh" size={26} />
              {/if}
            </span>
          </div>
          <div class="cycle-body">
            <strong>
              {running ? t("cleaning.running") : t("cleaning.transitioning")}
            </strong>
            <p>{running ? t("cleaning.runningHint") : t("cleaning.transitioningHint")}</p>
            <button class="stop" onclick={stop} disabled={busy}>
              <Icon name="close" size={15} />
              {t("cleaning.stop")}
            </button>
          </div>
        </div>
      {:else}
        <div class="idle">
          <div class="idle-body">
            <h2>{t("cleaning.readyTitle")}</h2>
            <p>{tm(status?.detail ?? null)}</p>

            {#if status && !status.supported}
              <!-- The distinction this page exists to keep visible: a
                   missing kernel module is not a verdict on the laptop. -->
              <p class="notice warn">
                {#if status.unreachable}
                  {t("cleaning.notAsked")}
                {:else}
                  {t("cleaning.notCapable")}
                {/if}
              </p>
            {/if}

            {#if tooHot && status}
              <p class="notice warn">
                {t("cleaning.tooHot", {
                  temp: formatTemp(status.cpuTempC),
                  limit: formatTemp(status.maxStartTempC),
                })}
              </p>
            {/if}
          </div>

          <button class="start" onclick={start} disabled={!canStart}>
            <Icon name="broom" size={17} />
            {busy ? t("cleaning.starting") : t("cleaning.start")}
          </button>
        </div>
      {/if}
    </Panel>

    <Panel title={t("cleaning.settings")}>
      <div class="setting">
        <span class="label">
          {t("cleaning.duration")}
          <InfoTip>{t("cleaning.durationHint")}</InfoTip>
        </span>
        <div class="control">
          <Slider
            value={seconds}
            min={SECONDS.min}
            max={SECONDS.max}
            disabled={active}
            minLabel={`${SECONDS.min}s`}
            maxLabel={`${SECONDS.max}s`}
            ariaLabel={t("cleaning.duration")}
            onchange={(v) => {
              seconds = v;
              void saveSettings();
            }}
          />
          <span class="digital value">{seconds}s</span>
        </div>
      </div>

      <div class="setting">
        <span class="label">
          {t("cleaning.customSpeed")}
          <InfoTip>{t("cleaning.customSpeedHint")}</InfoTip>
        </span>
        <div class="control">
          <Toggle
            checked={useCustomSpeed}
            disabled={active}
            onchange={(v) => {
              useCustomSpeed = v;
              void saveSettings();
            }}
            ariaLabel={t("cleaning.customSpeed")}
          />
          <span class="hint">
            {useCustomSpeed ? t("cleaning.speedChosen") : t("cleaning.speedFirmware")}
          </span>
        </div>
      </div>

      {#if useCustomSpeed}
        <div class="setting">
          <span class="label">{t("cleaning.speed")}</span>
          <div class="control">
            <Slider
              value={speed}
              min={SPEED.min}
              max={SPEED.max}
              disabled={active}
              minLabel={`${SPEED.min * 100}`}
              maxLabel={`${SPEED.max * 100}`}
              ariaLabel={t("cleaning.speed")}
              onchange={(v) => {
                speed = v;
                void saveSettings();
              }}
            />
            <span class="digital value">{speed * 100} rpm</span>
          </div>
        </div>
      {/if}
    </Panel>

    <!-- What the firmware said, for anyone whose machine refuses. It is
         the evidence behind the sentence above, and it is what a bug
         report needs. -->
    {#if status}
      <Panel title={t("cleaning.hardware")}>
        <ul class="facts">
          <li>
            <span>{t("cleaning.factGeneration")}</span>
            <span class="digital">
              {status.generation ? t(`cleaning.generation.${status.generation}`) : "-"}
            </span>
          </li>
          <li>
            <span>{t("cleaning.factFans")}</span>
            <span class="digital">
              {[
                status.capabilities.cpu && t("cleaning.fanCpu"),
                status.capabilities.gpu && t("cleaning.fanGpu"),
                status.capabilities.fan3 && t("cleaning.fanThird"),
              ]
                .filter(Boolean)
                .join(", ") || "-"}
            </span>
          </li>
          <li>
            <span>{t("cleaning.factAcpi")}</span>
            <span class="digital">
              {status.acpiCallLoaded
                ? t("cleaning.acpiLoaded")
                : status.acpiCallInstalled
                  ? t("cleaning.acpiNotLoaded")
                  : t("cleaning.acpiMissing")}
            </span>
          </li>
          <li>
            <span>{t("cleaning.factReversed")}</span>
            <span class="digital">{status.fansReversed ? t("common.yes") : t("common.no")}</span>
          </li>
        </ul>
      </Panel>
    {/if}
  </div>
</div>

<style>
  .cleaning {
    flex: 1;
    display: flex;
    flex-direction: column;
    min-height: 100%;
  }

  .stage {
    flex: 1;
    padding: 26px 30px 44px;
    display: flex;
    flex-direction: column;
    gap: 14px;
    max-width: 900px;
  }

  .title {
    font-size: 24px;
  }

  .lead {
    margin: -8px 0 0;
    color: var(--text-dim);
    font-size: 13px;
    line-height: 1.55;
  }

  .notice {
    margin: 10px 0 0;
    font-size: 13px;
    line-height: 1.5;
    color: var(--text-dim);
  }

  .notice.err {
    color: var(--danger);
  }

  .notice.warn {
    color: var(--warn);
  }

  /* --- a running cycle -------------------------------------------- */

  .cycle {
    display: flex;
    align-items: center;
    gap: 24px;
  }

  /* The remaining time as a ring, so the state is readable across the
     room - this is a control somebody walks away from. */
  .dial {
    position: relative;
    flex: 0 0 auto;
    display: grid;
    place-items: center;
    width: 96px;
    height: 96px;
    border-radius: 50%;
    background: conic-gradient(
      var(--accent-2) calc(var(--progress) * 360deg),
      #202024 0
    );
  }

  .dial::before {
    content: "";
    position: absolute;
    width: 74px;
    height: 74px;
    border-radius: 50%;
    background: var(--bg-panel);
  }

  .count {
    position: relative;
    font-size: 22px;
    color: var(--text);
  }

  .cycle-body strong {
    font-size: 16px;
  }

  .cycle-body p {
    margin: 6px 0 12px;
    color: var(--text-dim);
    font-size: 13px;
    line-height: 1.5;
    max-width: 52ch;
  }

  /* --- idle --------------------------------------------------------- */

  .idle {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 24px;
    flex-wrap: wrap;
  }

  .idle-body h2 {
    font-size: 16px;
    margin-bottom: 6px;
  }

  .idle-body p {
    margin: 0;
    color: var(--text-dim);
    font-size: 13px;
    line-height: 1.5;
    max-width: 56ch;
  }

  .start,
  .stop {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    padding: 10px 22px;
    border: none;
    border-radius: 2px;
    font-size: 12px;
    font-weight: 700;
    letter-spacing: 0.05em;
    text-transform: uppercase;
  }

  .start {
    background: #f2f2f4;
    color: #17171a;
  }

  .stop {
    background: var(--danger);
    color: #fff;
  }

  .start:disabled,
  .stop:disabled {
    opacity: 0.45;
  }

  /* --- settings ----------------------------------------------------- */

  .setting {
    display: grid;
    grid-template-columns: minmax(150px, 220px) 1fr;
    align-items: center;
    gap: 18px;
    padding: 10px 0;
  }

  .setting + .setting {
    border-top: 1px solid var(--line-soft);
  }

  .setting .label {
    color: var(--text-dim);
    font-size: 13px;
  }

  .control {
    display: flex;
    align-items: center;
    gap: 14px;
  }

  .control .hint {
    color: var(--text-mute);
    font-size: 12.5px;
  }

  .value {
    min-width: 82px;
    text-align: right;
    color: var(--text);
    font-size: 14px;
  }

  /* --- what the firmware said --------------------------------------- */

  .facts {
    list-style: none;
    margin: 0;
    padding: 0;
  }

  .facts li {
    display: flex;
    justify-content: space-between;
    gap: 16px;
    padding: 9px 0;
    border-bottom: 1px solid var(--line-soft);
    font-size: 13px;
    color: var(--text-dim);
  }

  .facts li:last-child {
    border-bottom: none;
  }

  .facts .digital {
    color: var(--text);
  }
</style>
