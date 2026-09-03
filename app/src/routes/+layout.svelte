<script lang="ts">
  /**
   * App shell: chrome, navigation and the notices that apply everywhere.
   * Telemetry polling is started once here so history keeps accumulating
   * while the user moves between pages.
   */
  import "$lib/styles/theme.css";
  import { onMount, type Snippet } from "svelte";
  import Sidebar from "$lib/components/Sidebar.svelte";
  import TitleBar from "$lib/components/TitleBar.svelte";
  import Banner from "$lib/components/Banner.svelte";
  import { settings } from "$lib/stores/settings.svelte";
  import { hardware } from "$lib/stores/hardware.svelte";
  import { telemetry } from "$lib/stores/telemetry.svelte";
  import { t } from "$lib/i18n/index.svelte";
  import { goto } from "$app/navigation";

  let { children }: { children: Snippet } = $props();

  let daemonNoticeDismissed = $state(false);
  let unsupportedNoticeDismissed = $state(false);

  // Cache first so the very first frame already has the user's language,
  // then the files on disk, which are authoritative.
  settings.loadCache();
  hardware.loadCache();

  // Deliberately `onMount` and not `$effect`: this block reads settings
  // (`start()` needs the poll interval) *and* writes them (`hydrate()`
  // replaces `settings.current`). As an effect that is a cycle - every
  // hydrate re-ran the block, which re-polled, re-hydrated, and so on
  // until the page froze. Startup happens once, so say so.
  onMount(() => {
    void settings.hydrate();
    void hardware.hydrate().then(() => hardware.syncFromDaemon());
    telemetry.start();
    void telemetry.loadSystemInfo();
    // Started here rather than per-page: the mode can change while any
    // page is open, and the sidebar and home dashboard show it too.
    const stopWatching = hardware.watchDaemon();
    return () => {
      telemetry.stop();
      stopWatching();
    };
  });

  /** Debounced writes could otherwise be lost when the window closes. */
  function flushSettings() {
    void settings.flush();
    void hardware.flush();
  }

  // TODO item: on launch, warn when the kernel driver is missing and offer
  // a shortcut to the drivers page, with a "don't show again" the user's
  // choice is remembered for.
  //
  // Only worth saying on hardware the driver could actually serve - on a
  // non-HP machine it isn't a missing driver, it's the wrong laptop, which
  // the unsupported notice below covers instead.
  const showDriverNotice = $derived(
    !telemetry.demo &&
      !telemetry.driverInstalled &&
      telemetry.systemInfo?.supported === true &&
      !settings.current.hideDriverNotice,
  );

  const showUnsupportedNotice = $derived(
    !telemetry.demo &&
      telemetry.systemInfo?.compatibility === "unsupported" &&
      !unsupportedNoticeDismissed,
  );
</script>

<svelte:window onbeforeunload={flushSettings} onpagehide={flushSettings} />

<div class="shell">
  <TitleBar />
  <div class="body">
    <Sidebar />
    <main class="content">
      {#if telemetry.demo && !daemonNoticeDismissed}
        <Banner
          kind="info"
          title={t("notices.daemonDownTitle")}
          dismissible
          ondismiss={() => (daemonNoticeDismissed = true)}
        >
          {t("notices.daemonDownBody")}
        </Banner>
      {/if}

      {#if showDriverNotice}
        <Banner kind="warning" title={t("notices.driverMissingTitle")}>
          {t("notices.driverMissingBody")}
          {#snippet actions()}
            <button class="link" onclick={() => goto("/drivers")}>
              {t("notices.goToDrivers")}
            </button>
            <label class="dismiss">
              <input
                type="checkbox"
                onchange={(e) => settings.set("hideDriverNotice", e.currentTarget.checked)}
              />
              {t("notices.dontShowAgain")}
            </label>
          {/snippet}
        </Banner>
      {/if}

      {#if showUnsupportedNotice}
        <Banner
          kind="warning"
          title={t("notices.unsupportedTitle")}
          dismissible
          ondismiss={() => (unsupportedNoticeDismissed = true)}
        >
          {telemetry.systemInfo?.reason ?? t("notices.unsupportedBody")}
        </Banner>
      {/if}

      <div class="page">
        {@render children()}
      </div>
    </main>
  </div>
</div>

<style>
  .shell {
    display: flex;
    flex-direction: column;
    height: 100vh;
    background: var(--bg-window);
  }

  .body {
    flex: 1;
    display: flex;
    min-height: 0;
  }

  .content {
    flex: 1;
    display: flex;
    flex-direction: column;
    min-width: 0;
  }

  .page {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
  }

  .link {
    border: none;
    background: transparent;
    color: #ffd9a0;
    text-decoration: underline;
    padding: 0;
    font-size: 13px;
  }

  .dismiss {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    white-space: nowrap;
    cursor: pointer;
  }
</style>
