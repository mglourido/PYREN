<script lang="ts">
  /**
   * Advanced performance tuning (GPU overclocking).
   *
   * Offsets are staged locally and only sent on Apply - an overclock
   * applied on every slider tick would be both pointless and risky.
   */
  import Icon from "$lib/components/Icon.svelte";
  import InfoTip from "$lib/components/InfoTip.svelte";
  import Legend from "$lib/components/Legend.svelte";
  import Segmented from "$lib/components/Segmented.svelte";
  import Slider from "$lib/components/Slider.svelte";
  import Sparkline from "$lib/components/Sparkline.svelte";
  import { t } from "$lib/i18n/index.svelte";
  import { formatTemp } from "$lib/stores/settings.svelte";
  import { LIMITS, hardware } from "$lib/stores/hardware.svelte";
  import { telemetry, type Series } from "$lib/stores/telemetry.svelte";

  let target = $state("gpu");
  let coreOffset = $state(hardware.state.gpuCoreOffset);
  let memOffset = $state(hardware.state.gpuMemOffset);

  const dirty = $derived(
    coreOffset !== hardware.state.gpuCoreOffset || memOffset !== hardware.state.gpuMemOffset,
  );

  function apply() {
    hardware.set("gpuCoreOffset", coreOffset);
    hardware.set("gpuMemOffset", memOffset);
  }

  function revert() {
    coreOffset = hardware.state.gpuCoreOffset;
    memOffset = hardware.state.gpuMemOffset;
  }

  const charts: Series[][] = $derived([
    [{ label: t("advanced.temperature"), color: "#e5178c", values: telemetry.cpuTempHistory }],
    [{ label: t("advanced.power"), color: "#2fd0ff", values: telemetry.gpuUsageHistory }],
    [{ label: t("advanced.speed"), color: "#ff8a00", values: telemetry.gpuUsageHistory }],
    [{ label: t("advanced.voltage"), color: "#b14cff", values: telemetry.cpuUsageHistory }],
  ]);
</script>

<div class="advanced">
  <header class="head">
    <Segmented
      value={target}
      options={[
        { value: "gpu", label: "GPU" },
        { value: "cpu", label: "CPU", disabled: true },
      ]}
      onchange={(v) => (target = v)}
    />
  </header>

  <section class="panel">
    <h1 class="title">
      {t("advanced.gpuOverclocking")}
      <InfoTip>{t("advanced.disclaimer")}</InfoTip>
    </h1>

    <div class="body">
      <div class="charts">
        {#each charts as series, i (i)}
          <Sparkline {series} max={100} height={78} columns={8} />
        {/each}
      </div>

      <dl class="stats">
        <dt>{t("advanced.temperature")}</dt>
        <dd class="v pink">{formatTemp(telemetry.gpuTempC, false)} <small>°C</small></dd>
        <dt>{t("advanced.power")}</dt>
        <dd class="v cyan">-- <small>W</small></dd>
        <dt>{t("advanced.speed")}</dt>
        <dd class="v orange">-- <small>GHz</small></dd>
        <dt>{t("advanced.voltage")}</dt>
        <dd class="v purple">-- <small>V</small></dd>
      </dl>

      <div class="sliders">
        <div class="slider-row">
          <span class="slider-label">{t("advanced.coreOffset")}</span>
          <Slider
            value={coreOffset}
            min={LIMITS.gpuCoreOffset.min}
            max={LIMITS.gpuCoreOffset.max}
            gradient={false}
            ariaLabel={t("advanced.coreOffset")}
            onchange={(v) => (coreOffset = v)}
          />
          <span class="slider-value">{coreOffset} MHz</span>
        </div>

        <div class="slider-row">
          <span class="slider-label">{t("advanced.memoryOffset")}</span>
          <Slider
            value={memOffset}
            min={LIMITS.gpuMemOffset.min}
            max={LIMITS.gpuMemOffset.max}
            step={5}
            gradient={false}
            ariaLabel={t("advanced.memoryOffset")}
            onchange={(v) => (memOffset = v)}
          />
          <span class="slider-value">{memOffset} MHz</span>
        </div>

        <p class="warn"><Icon name="warning" size={15} /> {t("advanced.disclaimer")}</p>

        <div class="actions">
          <button class="ghost" disabled={!dirty} onclick={revert}>{t("common.cancel")}</button>
          <button class="primary" disabled={!dirty} onclick={apply}>{t("common.apply")}</button>
        </div>
      </div>
    </div>
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
