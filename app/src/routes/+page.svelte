<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { onMount } from "svelte";

  type FanStatus = {
    driverInstalled: boolean;
    cpuTempC: number | null;
    fanRpm: number;
    isReverse: boolean;
  };

  let fanStatus = $state<FanStatus | null>(null);
  let daemonError = $state<string | null>(null);

  async function refresh() {
    try {
      fanStatus = await invoke<FanStatus>("fan_get_status");
      daemonError = null;
    } catch (e) {
      daemonError = String(e);
      fanStatus = null;
    }
  }

  onMount(() => {
    refresh();
    const interval = setInterval(refresh, 2000);
    return () => clearInterval(interval);
  });
</script>

<main class="container">
  <h1>Omen Hub</h1>

  {#if daemonError}
    <div class="card error">
      <strong>Can't reach omen-hub-daemon.</strong>
      <p>{daemonError}</p>
      <p class="hint">
        Run <code>cargo run</code> in <code>daemon/daemon</code> first (dev mode uses
        <code>/tmp/omen-hub-daemon.sock</code>).
      </p>
    </div>
  {:else if fanStatus}
    <div class="card">
      <h2>Fan</h2>
      <dl>
        <dt>Driver installed</dt>
        <dd>{fanStatus.driverInstalled ? "yes" : "no"}</dd>
        <dt>CPU temp</dt>
        <dd>{fanStatus.cpuTempC !== null ? `${fanStatus.cpuTempC} C` : "n/a"}</dd>
        <dt>Fan speed</dt>
        <dd>{fanStatus.fanRpm} RPM{fanStatus.isReverse ? " (reverse)" : ""}</dd>
      </dl>
    </div>
  {:else}
    <p>Loading...</p>
  {/if}
</main>

<style>
  :root {
    color-scheme: dark;
    font-family: Inter, Avenir, Helvetica, Arial, sans-serif;
    color: #e8e8e8;
    background-color: #17181c;
  }

  .container {
    margin: 0 auto;
    max-width: 640px;
    padding: 4rem 1.5rem;
  }

  h1 {
    margin-bottom: 0.25rem;
  }

  .subtitle {
    color: #9a9ca3;
    margin-top: 0;
  }

  .card {
    margin-top: 2rem;
    padding: 1.5rem;
    border-radius: 12px;
    background: #1f2126;
    border: 1px solid #2c2e35;
  }

  .card.error {
    border-color: #6e2b2b;
    background: #241a1a;
  }

  .hint {
    color: #9a9ca3;
    font-size: 0.9rem;
  }

  code {
    background: #0f1013;
    padding: 0.1rem 0.4rem;
    border-radius: 4px;
  }

  dl {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 0.5rem 1rem;
    margin: 0;
  }

  dt {
    color: #9a9ca3;
  }

  dd {
    margin: 0;
    font-weight: 600;
  }
</style>
