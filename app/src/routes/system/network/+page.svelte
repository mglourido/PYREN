<script lang="ts">
  /**
   * Network booster. System-wide smart queuing only - Off just monitors,
   * Auto hands the default-route interface `cake` (or `fq_codel` as a
   * fallback) so responsive traffic stays snappy under load. There is no
   * per-application priority or block list: that needs per-process traffic
   * accounting (cgroups/nftables/eBPF) this daemon does not implement -
   * see `dev/TODO.md` §2.1 and `daemon/crates/network/src/lib.rs`.
   */
  import Segmented from "$lib/components/Segmented.svelte";
  import { t } from "$lib/i18n/index.svelte";
  import { hardware, type NetworkMode } from "$lib/stores/hardware.svelte";
  import { telemetry } from "$lib/stores/telemetry.svelte";

  const mode = $derived(hardware.state.networkMode);
  const status = $derived(hardware.network);
  const total = $derived(telemetry.netUpMbps + telemetry.netDownMbps);
</script>

<div class="network">
  <header class="head">
    <div class="mode">
      <span class="label">{t("network.mode")}</span>
      <Segmented
        value={mode}
        options={[
          { value: "off", label: t("common.off") },
          { value: "auto", label: t("common.auto") },
        ]}
        onchange={(v) => hardware.setNetworkMode(v as NetworkMode)}
      />
    </div>

    <p class="desc">
      {t(mode === "off" ? "network.descOff" : "network.descAuto")}
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

    <div class="note">
      <p>{t("network.perAppUnavailable")}</p>
    </div>
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
    background: #1f1f23;
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
    border: 1px solid #55555c;
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

  .note {
    flex: 1;
    min-width: 260px;
    display: flex;
    align-items: flex-start;
    padding-top: 26px;
  }

  .note p {
    display: flex;
    align-items: flex-start;
    gap: 8px;
    margin: 0;
    max-width: 46ch;
    color: var(--text-dim);
    font-size: 13px;
    line-height: 1.5;
  }
</style>
