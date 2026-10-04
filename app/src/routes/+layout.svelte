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
  import NotificationsPanel from "$lib/components/NotificationsPanel.svelte";
  import { settings } from "$lib/stores/settings.svelte";
  import { hardware } from "$lib/stores/hardware.svelte";
  import { lightingPresets } from "$lib/stores/lighting-presets.svelte";
  import { isDetailRoute, telemetry } from "$lib/stores/telemetry.svelte";
  import { notifications } from "$lib/stores/notifications.svelte";
  import { maybeAutoCheckForUpdate } from "$lib/version";
  import { t, tm } from "$lib/i18n/index.svelte";
  import { goto } from "$app/navigation";
  import { page } from "$app/state";
  import { debugLog } from "$lib/api/debug";
  import { driverIdentityName } from "$lib/api/daemon";

  let { children }: { children: Snippet } = $props();

  let daemonNoticeDismissed = $state(false);
  let unsupportedNoticeDismissed = $state(false);
  // Component state, so closing the notice silences it for this run of the
  // app and the next launch shows it again while the driver is still out of
  // date. Silencing it for good is a separate, deliberate choice - the
  // "don't show again" box, `hideDriverOutdatedNotice` - because that one
  // hides every later driver update as well.
  let driverOutdatedNoticeDismissed = $state(false);

  // Cache first so the very first frame already has the user's language,
  // then the files on disk, which are authoritative.
  settings.loadCache();
  hardware.loadCache();
  lightingPresets.loadCache();

  // Ordered before `onMount` below: effects and `onMount` both run in
  // source order during component initialization, and `onMount`'s
  // `telemetry.start()` fires an immediate poll that reads `detailActive`.
  // Running this first means that poll already sees the right route
  // instead of racing `setDetailActive`'s own immediate poll, which
  // `Telemetry`'s in-flight guard would silently drop.
  $effect(() => {
    telemetry.setDetailActive(isDetailRoute(page.url.pathname));
  });

  // Asked when the daemon becomes reachable rather than once in `onMount`:
  // an app opened before its daemon would otherwise never learn the answer.
  // `demo` is the only thing read here - `loadDriverVersion` touches no
  // state before its first `await` - so this runs once per connection.
  $effect(() => {
    if (!telemetry.demo) void telemetry.loadDriverVersion();
  });

  // Deliberately `onMount` and not `$effect`: this block reads settings
  // (`start()` needs the poll interval) *and* writes them (`hydrate()`
  // replaces `settings.current`). As an effect that is a cycle - every
  // hydrate re-ran the block, which re-polled, re-hydrated, and so on
  // until the page froze. Startup happens once, so say so.
  onMount(() => {
    void settings.hydrate();
    void hardware.hydrate().then(() => hardware.syncFromDaemon());
    void lightingPresets.hydrate();
    telemetry.start();
    void telemetry.loadSystemInfo();
    // Started here rather than per-page: the mode can change while any
    // page is open, and the sidebar and home dashboard show it too.
    const stopWatching = hardware.watchDaemon();
    // The notification history follows the same event bus, and the header
    // bell is on every page.
    const stopNotifications = notifications.start();
    const stopErrorCapture = debugLog.installErrorCapture();
    // No timer: reads the stamped `lastUpdateCheckAt` and skips if it's
    // been under 6h, so this only ever does something once per session at
    // most - see `maybeAutoCheckForUpdate`.
    void maybeAutoCheckForUpdate();
    return () => {
      telemetry.stop();
      stopWatching();
      stopNotifications();
      stopErrorCapture();
    };
  });

  /** Debounced writes could otherwise be lost when the window closes. */
  function flushSettings() {
    void settings.flush();
    void hardware.flush();
    void lightingPresets.flush();
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

  // The installed driver is not the one this version of Pyren ships -
  // which is what updating the app leaves behind, since nothing rebuilds a
  // kernel module on its own. Only `outdated`: a stock driver is the notice
  // above's business, and an install that could not be identified is not
  // something to send anybody to reinstall over.
  const outdatedDriver = $derived(
    !telemetry.demo && telemetry.driverVersion?.state === "outdated"
      ? telemetry.driverVersion
      : null,
  );
  const showDriverOutdatedNotice = $derived(
    outdatedDriver !== null &&
      !driverOutdatedNoticeDismissed &&
      !settings.current.hideDriverOutdatedNotice,
  );

  const showUnsupportedNotice = $derived(
    !telemetry.demo &&
      telemetry.systemInfo?.compatibility === "unsupported" &&
      !unsupportedNoticeDismissed,
  );
</script>

<!-- Belt to the CSS's braces: kill any drag gesture the `-webkit-user-drag`
     rules miss (dragging a text selection, mostly). -->
<svelte:document ondragstart={(e) => e.preventDefault()} />

<svelte:window onbeforeunload={flushSettings} onpagehide={flushSettings} />

<div class="shell">
  <TitleBar />
  <NotificationsPanel />
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

      {#if showDriverOutdatedNotice && outdatedDriver?.installed && outdatedDriver.bundled}
        <Banner
          kind="info"
          title={t("notices.driverOutdatedTitle")}
          dismissible
          ondismiss={() => (driverOutdatedNoticeDismissed = true)}
        >
          {t("notices.driverOutdatedBody", {
            installed: driverIdentityName(outdatedDriver.installed),
            bundled: driverIdentityName(outdatedDriver.bundled),
          })}
          {#snippet actions()}
            <button class="link on-info" onclick={() => goto("/drivers")}>
              {t("notices.goToDriverUpdate")}
            </button>
            <label class="dismiss">
              <input
                type="checkbox"
                onchange={(e) =>
                  settings.set("hideDriverOutdatedNotice", e.currentTarget.checked)}
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
          {telemetry.systemInfo?.reason
            ? tm(telemetry.systemInfo.reason)
            : t("notices.unsupportedBody")}
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
    /* Safety net: a route's own grid can still hit a `minmax()` floor
       narrower than the actual window (tiling WMs routinely ignore the
       Tauri `minWidth` hint). A scrollbar here beats the alternative -
       content silently clipped past the window edge with its hitboxes
       left behind at their unshrunk position. */
    overflow-x: auto;
  }

  .link {
    border: none;
    background: transparent;
    color: #ffd9a0;
    text-decoration: underline;
    padding: 0;
    font-size: 13px;
  }

  /* The amber above is picked for the warning strip; on the blue one it
     reads as a second, unrelated status. */
  .link.on-info {
    color: inherit;
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
