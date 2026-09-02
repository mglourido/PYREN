<script lang="ts">
  /**
   * System vitals. Two views behind the same tab, like the reference app:
   * a card dashboard, and a dense per-component readout with history
   * graphs. The view choice is a user setting so it survives restarts.
   */
  import Gauge from "$lib/components/Gauge.svelte";
  import Icon from "$lib/components/Icon.svelte";
  import InfoTip from "$lib/components/InfoTip.svelte";
  import Legend from "$lib/components/Legend.svelte";
  import Sparkline from "$lib/components/Sparkline.svelte";
  import Toggle from "$lib/components/Toggle.svelte";
  import { t } from "$lib/i18n/index.svelte";
  import { formatTemp, settings } from "$lib/stores/settings.svelte";
  import { telemetry, tempColor, type Series } from "$lib/stores/telemetry.svelte";
  import { hardware } from "$lib/stores/hardware.svelte";
  import type { GpuMetrics } from "$lib/api/daemon";

  const advanced = $derived(settings.current.vitalsAdvancedView);

  const cpuSeries = $derived<Series[]>([
    { label: t("vitals.usage"), color: "#2f8fff", values: telemetry.cpuUsageHistory },
    { label: t("vitals.temperature"), color: "#b14cff", values: telemetry.cpuTempHistory },
  ]);
  /** One series per GPU, so each block graphs its own chip. */
  function seriesFor(gpu: GpuMetrics): Series[] {
    return [
      {
        label: t("vitals.usage"),
        color: "#ffb020",
        values: telemetry.gpuHistories[gpu.name] ?? [],
      },
    ];
  }

  /**
   * What to call a GPU on a machine that has more than one. A hybrid laptop
   * runs the desktop on the integrated chip and games on the card, so
   * "GPU" alone would leave the user guessing which gauge is which.
   */
  function gpuHeading(gpu: GpuMetrics): string {
    if (telemetry.gpus.length < 2 || gpu.integrated === null) return "GPU";
    return gpu.integrated ? t("vitals.gpuIntegrated") : t("vitals.gpuDiscrete");
  }
  /** Per-core rows in the advanced view, excluding the package sensor. */
  const cpuCoreTemps = $derived(
    telemetry.temperatures.filter(
      (r) => r.label.toLowerCase().startsWith("core") && !r.label.toLowerCase().includes("package"),
    ),
  );

  const ramSeries = $derived<Series[]>([
    { label: t("vitals.usage"), color: "#2f8fff", values: telemetry.ramHistory },
  ]);

  const gb = (n: number) => `${n.toFixed(1)} GB`;
  const bytesToGb = (n: number) => n / 1e9;
</script>

<div class="vitals">
  <header class="toolbar">
    <div class="views">
      <span class="label">{t("vitals.mode")}</span>
      <div class="view-buttons">
        <button
          class:active={!advanced}
          title={t("vitals.basicView")}
          onclick={() => settings.set("vitalsAdvancedView", false)}
        >
          <Icon name="gauge" size={20} />
        </button>
        <button
          class:active={advanced}
          title={t("vitals.advancedView")}
          onclick={() => settings.set("vitalsAdvancedView", true)}
        >
          <Icon name="chip" size={20} />
        </button>
        <InfoTip>
          <ul class="legend-list">
            <li><span class="dot ok"></span>{t("vitals.legendGreen")}</li>
            <li><span class="dot warn"></span>{t("vitals.legendOrange")}</li>
            <li><span class="dot danger"></span>{t("vitals.legendRed")}</li>
          </ul>
        </InfoTip>
      </div>
    </div>

    <div class="unit">
      <span>{t("vitals.temperature")}</span>
      <Toggle
        checked={settings.current.tempUnit === "c"}
        onchange={(v) => settings.set("tempUnit", v ? "c" : "f")}
        labelOff="°F"
        labelOn="°C"
        ariaLabel={t("settings.temperatureUnit")}
      />
    </div>
  </header>

  {#if !advanced}
    <div class="grid">
      <!-- One card per GPU: a hybrid machine has two, and which of them is
           working is exactly what this page is asked. -->
      {#each telemetry.gpus as gpu (gpu.name)}
        <section class="card">
          <h2>{gpuHeading(gpu)}</h2>
          <Gauge value={gpu.usagePercent} label={t("vitals.gpuUsage")} id="gpu-{gpu.name}" />
          <p class="chip-name" title={gpu.name}>{gpu.name}</p>
          <div class="foot">
            <span class="digital" style="color:{tempColor(gpu.tempC)}">
              {formatTemp(gpu.tempC)}
            </span>
            <small>{t("vitals.gpuTemp")}</small>
          </div>
        </section>
      {:else}
        <section class="card">
          <h2>GPU</h2>
          <Gauge value={telemetry.gpuUsage} label={t("vitals.gpuUsage")} id="gpu" />
          <div class="foot">
            <span class="digital" style="color:{tempColor(telemetry.gpuTempC)}">
              {formatTemp(telemetry.gpuTempC)}
            </span>
            <small>{t("vitals.gpuTemp")}</small>
          </div>
        </section>
      {/each}

      <section class="card">
        <h2>CPU</h2>
        <Gauge value={telemetry.cpuUsage} label={t("vitals.cpuUsage")} id="cpu" />
        <div class="foot">
          <span class="digital" style="color:{tempColor(telemetry.cpuTempC)}">
            {formatTemp(telemetry.cpuTempC)}
          </span>
          <small>{t("vitals.cpuTemp")}</small>
        </div>
      </section>

      <section class="card">
        <h2>RAM</h2>
        <Gauge value={telemetry.ramPercent} label={t("vitals.ramUsage")} id="ram" />
        <div class="foot">
          <small>{gb(telemetry.ramUsedGb)} / {gb(telemetry.ramTotalGb)}</small>
        </div>
      </section>

      <section class="card">
        <h2>{t("vitals.storage")}</h2>
        <ul class="disks">
          {#each telemetry.disks as disk (disk.mount)}
            <li>
              <span class="mount" title="{disk.device} ({disk.fstype})">{disk.mount}</span>
              <span class="bar">
                <span
                  class="fill"
                  style="width:{((disk.totalBytes - disk.freeBytes) / disk.totalBytes) * 100}%"
                ></span>
              </span>
              <small>
                {t("vitals.freeOf", {
                  free: gb(bytesToGb(disk.freeBytes)),
                  total: gb(bytesToGb(disk.totalBytes)),
                })}
              </small>
            </li>
          {:else}
            <li class="none"><small>{t("common.unavailable")}</small></li>
          {/each}
        </ul>
      </section>

      <section class="card">
        <h2>{t("vitals.yourConfig")}</h2>
        <p class="config-label">{t("tabs.performance")}</p>
        <a class="config-chip" href="/system/performance">
          {t(`performance.modes.${hardware.state.powerMode}`)}
        </a>
      </section>

      <section class="card">
        <h2>{t("vitals.network")}</h2>
        <div class="net">
          <span class="net-value">{telemetry.netUpMbps.toFixed(1)}</span>
          <small>{t("vitals.uploadSpeed")}<br />Mbps</small>
        </div>
        <div class="net">
          <span class="net-value">{telemetry.netDownMbps.toFixed(1)}</span>
          <small>{t("vitals.downloadSpeed")}<br />Mbps</small>
        </div>
        <p class="config-label">{t("home.networkBooster")}</p>
        <a class="config-chip" href="/system/network">
          {t(`common.${hardware.state.networkMode === "off" ? "disabled" : "enabled"}`)}
        </a>
      </section>

      <section class="card wide">
        <h2>{t("vitals.topProcesses")}</h2>
        <table>
          <thead>
            <tr>
              <th>{t("vitals.process")}</th>
              <th>CPU</th>
              <th>GPU</th>
              <th>RAM</th>
              <th>{t("vitals.action")}</th>
            </tr>
          </thead>
          <tbody>
            {#each telemetry.processes as process (process.pid)}
              <tr>
                <td class="proc-name" title="PID {process.pid}">{process.name}</td>
                <td>{process.cpuPercent.toFixed(1)} %</td>
                <td>
                  {process.gpuPercent === null ? "--" : `${process.gpuPercent.toFixed(1)} %`}
                </td>
                <td>{process.memMb.toFixed(0)} MB</td>
                <td><span class="mute">{process.pid}</span></td>
              </tr>
            {:else}
              <tr class="empty">
                <td colspan="5">
                  {telemetry.demo ? t("notices.daemonDownBody") : t("common.loading")}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </section>
    </div>
  {:else}
    <div class="advanced">
      <section class="block">
        <h2 class="block-title"><Icon name="chevronDown" size={16} /> CPU</h2>
        <p class="model">{telemetry.systemInfo?.cpu ?? "Intel Core Ultra 7 255H"}</p>
        <div class="cols">
          <dl>
            <dt class="col-head">{t("vitals.clock")}</dt>
            {#each telemetry.coreClocksMhz as mhz, i (i)}
              <dd class="row">
                <span>{t("vitals.core")} {i + 1}</span><b>{mhz.toFixed(0)} MHz</b>
              </dd>
            {/each}
          </dl>
          <dl>
            <dt class="col-head">{t("vitals.temperature")}</dt>
            <dd class="row">
              <span>{t("vitals.package")}</span>
              <b style="color:{tempColor(telemetry.cpuTempC)}">{formatTemp(telemetry.cpuTempC)}</b>
            </dd>
            {#each cpuCoreTemps as reading (reading.label)}
              <dd class="row">
                <span>{reading.label}</span>
                <b style="color:{tempColor(reading.celsius)}">{formatTemp(reading.celsius)}</b>
              </dd>
            {/each}
          </dl>
          <dl>
            <dt class="col-head">{t("vitals.usage")}</dt>
            <dd class="row"><span>{t("vitals.package")}</span><b>{telemetry.cpuUsage.toFixed(1)} %</b></dd>
            {#each telemetry.perCoreUsage as usage, i (i)}
              <dd class="row"><span>{t("vitals.core")} {i + 1}</span><b>{usage.toFixed(1)} %</b></dd>
            {/each}
          </dl>
          <div class="chart">
            <Sparkline series={cpuSeries} max={100} />
            <Legend series={cpuSeries} />
          </div>
        </div>
      </section>

      {#each telemetry.gpus as gpu (gpu.name)}
        <section class="block">
          <h2 class="block-title"><Icon name="chevronDown" size={16} /> {gpuHeading(gpu)}</h2>
          <p class="model">{gpu.name}</p>
          <div class="cols">
            <dl>
              <dt class="col-head">{t("vitals.clock")}</dt>
              <dd class="row">
                <span>{t("vitals.core")}</span>
                <b>{gpu.clockMhz === null ? "N/A" : `${gpu.clockMhz.toFixed(0)} MHz`}</b>
              </dd>
              <dt class="col-head">{t("vitals.memory")}</dt>
              <dd class="row">
                <span>{t("vitals.inUse")}</span>
                <b>{gpu.memUsedMb === null ? "N/A" : `${(gpu.memUsedMb / 1024).toFixed(1)} GB`}</b>
              </dd>
              <dd class="row">
                <span>{t("vitals.total")}</span>
                <b>{gpu.memTotalMb === null ? "N/A" : `${(gpu.memTotalMb / 1024).toFixed(1)} GB`}</b>
              </dd>
            </dl>
            <dl>
              <dt class="col-head">{t("vitals.temperature")}</dt>
              <dd class="row">
                <span>{t("vitals.core")}</span>
                <b style="color:{tempColor(gpu.tempC)}">{formatTemp(gpu.tempC)}</b>
              </dd>
              <dt class="col-head">{t("vitals.power")}</dt>
              <dd class="row">
                <span>{t("vitals.package")}</span>
                <b>{gpu.powerW === null ? "N/A" : `${gpu.powerW.toFixed(1)} W`}</b>
              </dd>
            </dl>
            <dl>
              <dt class="col-head">{t("vitals.usage")}</dt>
              <dd class="row">
                <span>{t("vitals.core")}</span>
                <b>{gpu.usagePercent === null ? "N/A" : `${gpu.usagePercent.toFixed(1)} %`}</b>
              </dd>
            </dl>
            <div class="chart">
              <Sparkline series={seriesFor(gpu)} max={100} />
              <Legend series={seriesFor(gpu)} />
            </div>
          </div>
        </section>
      {/each}

      <section class="block">
        <h2 class="block-title"><Icon name="chevronDown" size={16} /> RAM</h2>
        <div class="cols">
          <dl>
            <dt class="col-head">{t("vitals.total")}</dt>
            <dd class="row"><span>{t("vitals.inUse")}</span><b>{gb(telemetry.ramUsedGb)}</b></dd>
            <dd class="row">
              <span>{t("vitals.available")}</span>
              <b>{gb(telemetry.ramTotalGb - telemetry.ramUsedGb)}</b>
            </dd>
            <dd class="row"><span>{t("vitals.total")}</span><b>{gb(telemetry.ramTotalGb)}</b></dd>
            <dd class="row">
              <span>{t("vitals.usage")}</span><b>{telemetry.ramPercent.toFixed(1)} %</b>
            </dd>
            {#if telemetry.swapTotalGb > 0}
              <dt class="col-head">Swap</dt>
              <dd class="row">
                <span>{t("vitals.inUse")}</span><b>{gb(telemetry.swapUsedGb)}</b>
              </dd>
              <dd class="row">
                <span>{t("vitals.total")}</span><b>{gb(telemetry.swapTotalGb)}</b>
              </dd>
            {/if}
          </dl>
          <dl></dl>
          <div class="chart">
            <Sparkline series={ramSeries} max={100} />
            <Legend series={ramSeries} />
          </div>
        </div>
      </section>
    </div>
  {/if}
</div>

<style>
  .vitals {
    display: flex;
    flex-direction: column;
  }

  .toolbar {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: 20px;
    padding: 14px 26px;
    background: #1f1f23;
    border-bottom: 1px solid var(--line-soft);
  }

  .label {
    display: block;
    color: var(--text-dim);
    font-size: 14px;
    margin-bottom: 6px;
  }

  .view-buttons {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .view-buttons button {
    display: grid;
    place-items: center;
    width: 38px;
    height: 34px;
    border: none;
    border-radius: 2px;
    background: #131316;
    color: var(--text-dim);
  }

  .view-buttons button.active {
    background: #f2f2f4;
    color: #17171a;
  }

  .unit {
    display: flex;
    align-items: center;
    gap: 14px;
    color: var(--text-dim);
    font-size: 14px;
  }

  .legend-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .legend-list li {
    display: flex;
    gap: 9px;
    align-items: flex-start;
  }

  .dot {
    flex: 0 0 auto;
    width: 9px;
    height: 9px;
    margin-top: 5px;
    border-radius: 50%;
  }

  .dot.ok {
    background: var(--ok);
  }
  .dot.warn {
    background: var(--warn);
  }
  .dot.danger {
    background: var(--danger);
  }

  .grid {
    flex: 1;
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(250px, 1fr));
    gap: 14px;
    padding: 20px 26px 32px;
    background: var(--omen-black);
  }

  .card {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 12px;
    padding: 18px;
    background: #242428;
    border-radius: var(--radius-sm);
  }

  .card h2 {
    align-self: flex-start;
    font-size: 19px;
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }

  .card.wide {
    grid-column: span 2;
    align-items: stretch;
  }

  .chip-name {
    align-self: flex-start;
    margin: 0;
    color: var(--text-dim);
    font-size: 12px;
    /* Card names run long ("Intel Corporation Arrow Lake-P [Arc Pro
       130T/140T]"); one clipped line beats a card that grows to fit. */
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .foot {
    align-self: flex-start;
    display: flex;
    flex-direction: column;
    gap: 3px;
    margin-top: auto;
  }

  .foot .digital {
    font-size: 22px;
  }

  .foot small,
  .card small {
    color: var(--text-dim);
    font-size: 12px;
  }

  .disks {
    align-self: stretch;
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 14px;
  }

  .disks li {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 4px 10px;
    align-items: center;
  }

  .mount {
    font-size: 13px;
    color: var(--text-dim);
  }

  .bar {
    height: 12px;
    background: #6a6a72;
    border-radius: 2px;
    overflow: hidden;
  }

  .fill {
    display: block;
    height: 100%;
    background: var(--gradient);
  }

  .disks small {
    grid-column: 2;
  }

  .config-label {
    align-self: flex-start;
    margin: 0;
    color: var(--text-dim);
    font-size: 13px;
  }

  .config-chip {
    align-self: flex-start;
    padding: 7px 22px;
    border: 1px solid var(--accent-2);
    border-radius: 2px;
    font-size: 13px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    text-decoration: none;
  }

  .net {
    align-self: flex-start;
    display: flex;
    align-items: center;
    gap: 12px;
  }

  .net-value {
    font-size: 40px;
    font-weight: 300;
  }

  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 13px;
  }

  th {
    text-align: left;
    padding: 8px 10px;
    color: var(--text-dim);
    font-weight: 400;
    text-transform: uppercase;
    font-size: 12px;
    letter-spacing: 0.05em;
  }

  .empty td {
    padding: 24px 10px;
    color: var(--text-mute);
    text-align: center;
  }

  .advanced {
    padding: 22px 30px 40px;
    display: flex;
    flex-direction: column;
    gap: 34px;
  }

  .block-title {
    display: flex;
    align-items: center;
    gap: 10px;
    font-size: 19px;
    font-style: italic;
    letter-spacing: 0.03em;
  }

  .model {
    margin: 6px 0 14px 26px;
    font-size: 14px;
  }

  .cols {
    display: grid;
    grid-template-columns: repeat(4, minmax(150px, 1fr));
    gap: 26px;
    padding-left: 26px;
  }

  dl {
    margin: 0;
  }

  .col-head {
    font-weight: 700;
    font-size: 13px;
    margin-bottom: 8px;
  }

  .col-head:not(:first-child) {
    margin-top: 18px;
  }

  .row {
    display: flex;
    justify-content: space-between;
    gap: 20px;
    margin: 0;
    padding: 3px 0;
    max-width: 300px;
    font-size: 13px;
    color: var(--text-dim);
  }

  .row b {
    color: var(--text);
    font-weight: 500;
  }

  .chart {
    grid-column: 4;
  }

  .proc-name {
    color: var(--text);
  }

  .none {
    color: var(--text-mute);
  }
</style>
