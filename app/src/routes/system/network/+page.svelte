<script lang="ts">
  /**
   * Network booster. Two halves: a machine-wide mode - Off just monitors,
   * Auto hands the default-route interface `cake` (or `fq_codel` as a
   * fallback) - and a per-application table where each process name can be
   * blocked or given a send priority. The table is polled only while this
   * page is open, which is also what keeps the daemon sampling at all. See
   * `daemon/crates/network/src/lib.rs` for what each rule can and cannot do.
   */
  import { onMount } from "svelte";
  import Segmented from "$lib/components/Segmented.svelte";
  import {
    daemon,
    errorText,
    type NetworkAction,
    type NetworkProcesses,
  } from "$lib/api/daemon";
  import { t, tm } from "$lib/i18n/index.svelte";
  import { networkDescriptionKey } from "$lib/network/mode";
  import {
    MAX_PROCESS_NAME,
    NETWORK_ACTIONS,
    canAddRule,
    formatRate,
    ruleIsIdle,
  } from "$lib/network/processes";
  import { hardware, type NetworkMode } from "$lib/stores/hardware.svelte";
  import { telemetry } from "$lib/stores/telemetry.svelte";

  const POLL_MS = 1000;
  /** The mode is re-read far less often: each read costs the daemon a
   *  `tc` process, and it only changes behind the page's back when the
   *  daemon restores it or the default route moves. */
  const STATUS_POLL_MS = 5000;

  const mode = $derived(hardware.state.networkMode);
  const status = $derived(hardware.network);
  const total = $derived(telemetry.netUpMbps + telemetry.netDownMbps);

  let apps = $state<NetworkProcesses | null>(null);
  let appsError = $state<string | null>(null);
  /** A rule being written: its reply is newer than any poll in flight. */
  let writing = false;

  const hasIdleRule = $derived(
    apps?.processes.some((p) => ruleIsIdle(p.action, apps?.priorityActive ?? false)) ?? false,
  );

  async function refresh() {
    try {
      const listing = await daemon.networkProcesses();
      if (!writing) apps = listing;
      appsError = null;
    } catch (e) {
      appsError = errorText(e);
    }
  }

  async function setRule(name: string, action: NetworkAction) {
    writing = true;
    try {
      apps = await daemon.setNetworkRule(name, action);
      appsError = null;
    } catch (e) {
      appsError = errorText(e);
    } finally {
      writing = false;
    }
  }

  let newName = $state("");
  let newAction = $state<NetworkAction>("block");

  async function addRule() {
    const name = newName.trim();
    if (!canAddRule(name)) return;
    await setRule(name, newAction);
    if (!appsError) newName = "";
  }

  onMount(() => {
    let polling = false;
    const poll = async () => {
      if (polling) return;
      polling = true;
      await refresh();
      polling = false;
    };
    void poll();
    void hardware.refreshNetwork();
    const timer = setInterval(() => void poll(), POLL_MS);
    const statusTimer = setInterval(() => void hardware.refreshNetwork(), STATUS_POLL_MS);
    return () => {
      clearInterval(timer);
      clearInterval(statusTimer);
    };
  });

  // Whether priority rules are live depends on the mode, so a mode change
  // should not wait a poll to be reflected in the hint under the table.
  $effect(() => {
    void mode;
    void refresh();
  });
</script>

<div class="network">
  <header class="head">
    <div class="mode">
      <span class="label">{t("network.mode")}</span>
      <Segmented
        value={mode ?? ""}
        options={[
          { value: "off", label: t("common.off") },
          { value: "auto", label: t("common.auto") },
        ]}
        onchange={(v) => hardware.setNetworkMode(v as NetworkMode)}
      />
    </div>

    <p class="desc">
      {t(networkDescriptionKey(mode))}
    </p>

    {#if status}
      <div class="status">
        {#if status.interface}
          <span>{t("network.interface")}: <strong>{status.interface}</strong></span>
          <span>{t("network.activeQueuing")}: <strong>{status.activeQdisc ?? "-"}</strong></span>
        {:else}
          <span class="mute">{t("network.noInterface")}</span>
        {/if}
      </div>
    {/if}

    {#if hardware.lastError}
      <p class="error">{hardware.lastError}</p>
    {/if}
  </header>

  <div class="body">
    <aside class="total">
      <h2>{t("network.totalBandwidth")}</h2>
      <div class="dial">
        <span class="digits">
          {#each total.toFixed(2).split("") as ch, i (i)}
            {#if ch === "."}
              <span class="dot">,</span>
            {:else}
              <span class="digit">{ch}</span>
            {/if}
          {/each}
        </span>
        <small>Mbps</small>
      </div>
    </aside>

    <section class="apps">
      <h2>{t("network.apps.title")}</h2>
      {#if apps && !apps.available}
        <p class="mute">{tm(apps.reason)}</p>
      {:else if apps}
        <p class="hint">{t("network.apps.hint")}</p>
        {#if apps.processes.length === 0}
          <p class="mute">{t("network.apps.empty")}</p>
        {:else}
          <table>
            <thead>
              <tr>
                <th>{t("network.apps.process")}</th>
                <th class="num">{t("network.apps.download")}</th>
                <th class="num">{t("network.apps.upload")}</th>
                <th>{t("network.apps.rule")}</th>
              </tr>
            </thead>
            <tbody>
              {#each apps.processes as process (process.name)}
                <tr class:blocked={process.action === "block"}>
                  <td class="name">
                    {process.name}
                    {#if process.pids.length === 0}
                      <small>{t("network.apps.notRunning")}</small>
                    {/if}
                  </td>
                  <td class="num">{formatRate(process.downBps)}</td>
                  <td class="num">{formatRate(process.upBps)}</td>
                  <td>
                    <Segmented
                      value={process.action}
                      options={NETWORK_ACTIONS.map((action) => ({
                        value: action,
                        label: t(`network.apps.${action}`),
                      }))}
                      onchange={(v) => setRule(process.name, v as NetworkAction)}
                    />
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
        {/if}
        {#if hasIdleRule}
          <p class="hint warn">{t("network.apps.priorityIdle")}</p>
        {/if}
        <form
          class="add"
          onsubmit={(e) => {
            e.preventDefault();
            void addRule();
          }}
        >
          <input
            type="text"
            bind:value={newName}
            maxlength={MAX_PROCESS_NAME}
            placeholder={t("network.apps.addPlaceholder")}
            aria-label={t("network.apps.addPlaceholder")}
            spellcheck="false"
            autocomplete="off"
          />
          <Segmented
            value={newAction}
            options={NETWORK_ACTIONS.filter((action) => action !== "normal").map((action) => ({
              value: action,
              label: t(`network.apps.${action}`),
            }))}
            onchange={(v) => (newAction = v as NetworkAction)}
          />
          <button type="submit" disabled={!canAddRule(newName.trim())}>
            {t("network.apps.add")}
          </button>
        </form>
        <p class="hint">{t("network.apps.addHint")}</p>
      {/if}
      {#if appsError}
        <p class="error">{appsError}</p>
      {/if}
    </section>
  </div>
</div>

<style>
  .network {
    display: flex;
    flex-direction: column;
    min-height: 100%;
  }

  .head {
    display: flex;
    flex-direction: column;
    gap: 14px;
    padding: 16px 26px;
    background: var(--bg-chrome);
    border-bottom: 1px solid var(--line-soft);
  }

  .mode {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .label {
    color: var(--text-dim);
    font-size: 14px;
  }

  .desc {
    margin: 0;
    color: var(--text-dim);
    font-size: 13px;
    line-height: 1.4;
    max-width: 60ch;
  }

  .status {
    display: flex;
    gap: 22px;
    font-size: 13px;
    color: var(--text-dim);
  }

  .status strong {
    color: var(--text);
    font-weight: 500;
  }

  .mute {
    color: var(--text-mute);
  }

  .error {
    margin: 0;
    color: var(--danger, #e5484d);
    font-size: 13px;
  }

  .body {
    flex: 1;
    display: flex;
    flex-wrap: wrap;
    gap: 26px;
    padding: 26px;
    background: var(--omen-black);
  }

  .total {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 26px;
    padding: 26px 20px;
  }

  .total h2 {
    font-size: 17px;
    font-weight: 400;
    text-align: center;
  }

  .dial {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 10px;
    width: 240px;
    height: 240px;
    justify-content: center;
    border: 1px solid var(--line);
    border-radius: 50%;
  }

  .digits {
    display: flex;
    align-items: center;
    gap: 4px;
  }

  .digit {
    display: grid;
    place-items: center;
    width: 42px;
    height: 44px;
    border: 2px solid var(--text);
    border-radius: 3px;
    font-size: 28px;
    font-weight: 300;
  }

  .dot {
    font-size: 26px;
  }

  .dial small {
    color: var(--text-dim);
    font-size: 15px;
  }

  .apps {
    flex: 1;
    min-width: 320px;
    display: flex;
    flex-direction: column;
    gap: 14px;
    padding-top: 26px;
  }

  .apps h2 {
    margin: 0;
    font-size: 17px;
    font-weight: 400;
  }

  .hint {
    margin: 0;
    max-width: 60ch;
    color: var(--text-dim);
    font-size: 13px;
    line-height: 1.5;
  }

  .hint.warn {
    color: var(--text);
  }

  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 13px;
  }

  th {
    padding: 8px 10px;
    border-bottom: 1px solid var(--line);
    color: var(--text-dim);
    font-weight: 400;
    text-align: left;
  }

  td {
    padding: 6px 10px;
    border-bottom: 1px solid var(--line-soft);
  }

  .num {
    text-align: right;
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
  }

  .name small {
    margin-left: 8px;
    color: var(--text-mute);
  }

  .add {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 12px;
    margin-top: 8px;
  }

  .add input {
    width: 18ch;
    padding: 8px 10px;
    border: 1px solid var(--line);
    border-radius: 3px;
    background: transparent;
    color: var(--text);
    font: inherit;
    font-size: 13px;
  }

  .add button[type="submit"] {
    padding: 8px 16px;
    border: 1px solid var(--line);
    border-radius: 3px;
    background: transparent;
    color: var(--text);
    font-size: 12px;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    cursor: pointer;
  }

  .add button[type="submit"]:disabled {
    color: var(--text-mute);
    cursor: default;
  }

  tr.blocked .name,
  tr.blocked .num {
    color: var(--text-mute);
  }
</style>
