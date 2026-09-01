<script lang="ts">
  /**
   * Left navigation, following the reference app's split: a short list of
   * top-level destinations, then a per-device section that expands into the
   * same tabs the device pages show along the top.
   *
   * The store/marketing entries from the Windows app are deliberately not
   * reproduced - they have no Linux counterpart and would be dead links.
   */
  import { page } from "$app/state";
  import Icon from "./Icon.svelte";
  import { t } from "$lib/i18n/index.svelte";
  import { telemetry } from "$lib/stores/telemetry.svelte";
  import { DEVICE_TABS } from "$lib/nav";

  let deviceOpen = $state(true);

  const deviceName = $derived(
    telemetry.systemInfo?.model ?? (telemetry.demo ? "OMEN 16" : t("system.unknown")),
  );

  const main = [
    { href: "/", icon: "home", label: "nav.home" },
    { href: "/system/performance", icon: "gauge", label: "nav.performance" },
    { href: "/system/lighting", icon: "bulb", label: "nav.lighting" },
  ];

  const bottom = [
    { href: "/drivers", icon: "download", label: "nav.drivers" },
    { href: "/settings", icon: "settings", label: "nav.settings" },
    { href: "/help", icon: "help", label: "nav.help" },
  ];

  function isActive(href: string): boolean {
    return href === "/" ? page.url.pathname === "/" : page.url.pathname === href;
  }
</script>

<nav class="sidebar">
  <ul class="group">
    {#each main as item (item.href)}
      <li>
        <a href={item.href} class="item" class:active={isActive(item.href)}>
          <Icon name={item.icon} size={18} />
          <span>{t(item.label)}</span>
        </a>
      </li>
    {/each}
  </ul>

  <div class="divider"></div>

  <div class="device">
    <button class="item device-head" onclick={() => (deviceOpen = !deviceOpen)}>
      <Icon name="laptop" size={18} />
      <span class="device-name">{deviceName}</span>
      <Icon name={deviceOpen ? "chevronUp" : "chevronDown"} size={16} />
    </button>

    {#if deviceOpen}
      <ul class="group sub">
        {#each DEVICE_TABS as tab (tab.href)}
          <li>
            <a href={tab.href} class="item sub-item" class:active={isActive(tab.href)}>
              <span>{t(tab.label)}</span>
            </a>
          </li>
        {/each}
      </ul>
    {/if}
  </div>

  <div class="spacer"></div>

  <ul class="group">
    {#each bottom as item (item.href)}
      <li>
        <a href={item.href} class="item" class:active={isActive(item.href)}>
          <Icon name={item.icon} size={18} />
          <span>{t(item.label)}</span>
        </a>
      </li>
    {/each}
  </ul>

  <div class="status" class:offline={telemetry.demo}>
    <span class="dot"></span>
    <span>{telemetry.demo ? "demo" : "omen-hub-daemon"}</span>
  </div>
</nav>

<style>
  .sidebar {
    width: var(--sidebar-w);
    flex: 0 0 var(--sidebar-w);
    display: flex;
    flex-direction: column;
    background: var(--bg-chrome);
    border-right: 1px solid var(--line-soft);
    padding: 12px 0 0;
    overflow-y: auto;
  }

  .group {
    list-style: none;
    margin: 0;
    padding: 0 8px;
  }

  .item {
    position: relative;
    width: 100%;
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 9px 12px;
    border: none;
    background: transparent;
    color: var(--text-dim);
    text-decoration: none;
    border-radius: var(--radius-sm);
    font-size: 13px;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    text-align: left;
  }

  .item:hover {
    background: var(--bg-card);
    color: var(--text);
  }

  /* Selected rows get the brand gradient as a left rail plus a soft wash,
     the same treatment the reference app uses. */
  .item.active {
    color: var(--text);
    background: linear-gradient(90deg, rgba(229, 23, 140, 0.22), transparent 85%);
  }

  .item.active::before {
    content: "";
    position: absolute;
    left: 0;
    top: 6px;
    bottom: 6px;
    width: 3px;
    border-radius: 2px;
    background: var(--gradient-v);
  }

  .divider {
    height: 1px;
    background: var(--line-soft);
    margin: 12px 16px;
  }

  .device-head {
    justify-content: space-between;
  }

  .device-name {
    flex: 1;
  }

  .sub {
    padding: 2px 8px 4px 20px;
  }

  .sub-item {
    text-transform: none;
    letter-spacing: 0;
    font-size: 13px;
    padding: 7px 12px;
  }

  .spacer {
    flex: 1;
    min-height: 16px;
  }

  .status {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 12px 20px;
    border-top: 1px solid var(--line-soft);
    color: var(--text-mute);
    font-size: 11px;
  }

  .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--ok);
  }

  .status.offline .dot {
    background: var(--warn);
  }
</style>
