<script lang="ts">
  /**
   * Network booster. Off just monitors; Auto prioritises whatever is in the
   * foreground; Custom exposes a per-application priority the daemon would
   * turn into traffic-control rules.
   */
  import Icon from "$lib/components/Icon.svelte";
  import InfoTip from "$lib/components/InfoTip.svelte";
  import Segmented from "$lib/components/Segmented.svelte";
  import Toggle from "$lib/components/Toggle.svelte";
  import { t } from "$lib/i18n/index.svelte";
  import { hardware, type NetworkMode } from "$lib/stores/hardware.svelte";
  import { telemetry } from "$lib/stores/telemetry.svelte";

  type Priority = "high" | "medium" | "low";
  type App = { pid: number; name: string; down: number; up: number; priority: Priority; blocked: boolean };

  let query = $state("");
  let sortKey = $state<"name" | "down" | "up">("name");
  let doubleForce = $state(false);

  // Placeholder rows until the daemon reports per-process traffic; they
  // keep the table's behaviour (sorting, filtering, priority edits)
  // testable while that side is unimplemented.
  let apps = $state<App[]>([
    { pid: 1, name: "steam", down: 0, up: 0, priority: "medium", blocked: false },
    { pid: 2, name: "steamwebhelper", down: 0, up: 0, priority: "low", blocked: false },
    { pid: 3, name: "pyren", down: 0, up: 0, priority: "medium", blocked: false },
    { pid: 4, name: "firefox", down: 0, up: 0, priority: "medium", blocked: false },
    { pid: 5, name: "systemd-resolved", down: 0, up: 0, priority: "low", blocked: false },
  ]);

  const mode = $derived(hardware.state.networkMode);
  const editable = $derived(mode === "custom");

  const visible = $derived(
    apps
      .filter((app) => app.name.toLowerCase().includes(query.toLowerCase()))
      .sort((a, b) =>
        sortKey === "name" ? a.name.localeCompare(b.name) : b[sortKey] - a[sortKey],
      ),
  );

  function setPriority(pid: number, priority: Priority) {
    apps = apps.map((app) => (app.pid === pid ? { ...app, priority } : app));
  }

  function toggleBlock(pid: number) {
    apps = apps.map((app) => (app.pid === pid ? { ...app, blocked: !app.blocked } : app));
  }

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
          { value: "custom", label: t("common.custom") },
        ]}
        onchange={(v) => hardware.set("networkMode", v as NetworkMode)}
      />
    </div>

    <p class="desc">
      {t(mode === "off" ? "network.descOff" : mode === "auto" ? "network.descAuto" : "network.descCustom")}
    </p>

    <label class="search">
      <input placeholder={t("network.search")} bind:value={query} />
      <Icon name="search" size={16} />
    </label>

    {#if editable}
      <div class="force">
        <span class="force-label">
          {t("network.doubleForce")}
          <InfoTip align="right">{t("network.doubleForceHint")}</InfoTip>
        </span>
        <Toggle
          checked={doubleForce}
          onchange={(v) => (doubleForce = v)}
          labelOff={t("common.off")}
          labelOn={t("common.on")}
          ariaLabel={t("network.doubleForce")}
        />
      </div>
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

    <div class="table-wrap">
      <table>
        <thead>
          <tr>
            <th>
              <button onclick={() => (sortKey = "name")}>
                {t("network.appName")} <Icon name="chevronUp" size={13} />
              </button>
            </th>
            <th>
              <button onclick={() => (sortKey = "down")}>
                {t("network.download")} <Icon name="chevronUp" size={13} />
              </button>
            </th>
            <th>
              <button onclick={() => (sortKey = "up")}>
                {t("network.upload")} <Icon name="chevronUp" size={13} />
              </button>
            </th>
            <th>{t("network.priority")}</th>
            <th>{t("network.block")}</th>
          </tr>
        </thead>
        <tbody>
          {#each visible as app (app.pid)}
            <tr class:blocked={app.blocked}>
              <td class="name"><span class="favicon"></span>{app.name}</td>
              <td>{app.down} kbps</td>
              <td>{app.up} kbps</td>
              <td>
                {#if editable}
                  <select
                    value={app.priority}
                    onchange={(e) => setPriority(app.pid, e.currentTarget.value as Priority)}
                  >
                    <option value="high">{t("network.high")}</option>
                    <option value="medium">{t("network.medium")}</option>
                    <option value="low">{t("network.low")}</option>
                  </select>
                {:else}
                  <span class="mute">-</span>
                {/if}
              </td>
              <td>
                {#if editable}
                  <button class="block" onclick={() => toggleBlock(app.pid)}>
                    <Icon name={app.blocked ? "close" : "check"} size={15} />
                  </button>
                {:else}
                  <span class="mute">-</span>
                {/if}
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
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
    display: grid;
    grid-template-columns: auto minmax(220px, 1fr) 240px auto;
    gap: 22px;
    align-items: center;
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
  }

  .search {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 7px 12px;
    background: #2f2f34;
    border-radius: var(--radius-sm);
    color: var(--text-dim);
  }

  .search input {
    flex: 1;
    min-width: 0;
    border: none;
    background: transparent;
    color: var(--text);
    font: inherit;
    outline: none;
  }

  .force {
    display: flex;
    flex-direction: column;
    gap: 6px;
    font-size: 12px;
  }

  .force-label {
    display: flex;
    align-items: center;
    gap: 7px;
    font-weight: 600;
  }

  .body {
    flex: 1;
    display: grid;
    grid-template-columns: minmax(260px, 340px) 1fr;
    background: var(--omen-black);
  }

  .total {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 26px;
    padding: 26px 20px;
    border-right: 1px solid var(--line-soft);
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

  .table-wrap {
    overflow: auto;
  }

  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 14px;
  }

  th {
    position: sticky;
    top: 0;
    text-align: left;
    padding: 12px 16px;
    background: #1f1f23;
    font-weight: 400;
    color: var(--text-dim);
    white-space: nowrap;
  }

  th button {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    border: none;
    background: transparent;
    color: inherit;
    font: inherit;
    padding: 0;
  }

  td {
    padding: 10px 16px;
    border-bottom: 1px solid #17171a;
    color: var(--text-dim);
  }

  tr.blocked td {
    opacity: 0.45;
  }

  .name {
    display: flex;
    align-items: center;
    gap: 12px;
    color: var(--text);
  }

  .favicon {
    width: 18px;
    height: 18px;
    border-radius: 3px;
    background: var(--gradient);
  }

  .mute {
    color: var(--text-mute);
  }

  select {
    appearance: none;
    background-color: #2a2a2e;
    color: var(--text);
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    padding: 3px 26px 3px 8px;
    font: inherit;
    font-size: 13px;
    background-image: linear-gradient(45deg, transparent 50%, var(--text-dim) 50%),
      linear-gradient(135deg, var(--text-dim) 50%, transparent 50%);
    background-position:
      right 13px center,
      right 8px center;
    background-size:
      6px 6px,
      6px 6px;
    background-repeat: no-repeat;
  }

  /* The native popup list ignores the control's colours on some engines,
     so it needs its own dark background to match the theme. */
  select option {
    background: #2a2a2e;
    color: var(--text);
  }

  .block {
    display: grid;
    place-items: center;
    width: 26px;
    height: 26px;
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--text-dim);
  }
</style>
