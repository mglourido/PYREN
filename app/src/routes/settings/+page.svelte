<script lang="ts">
  import { onMount } from "svelte";
  import Panel from "$lib/components/Panel.svelte";
  import Slider from "$lib/components/Slider.svelte";
  import Toggle from "$lib/components/Toggle.svelte";
  import { availableLocales, localeName, t } from "$lib/i18n/index.svelte";
  import { settings } from "$lib/stores/settings.svelte";
  import { hardware } from "$lib/stores/hardware.svelte";
  import { telemetry } from "$lib/stores/telemetry.svelte";
  import { session, type SessionStatus } from "$lib/api/session";
  import { daemon, type HotkeyStatus } from "$lib/api/daemon";

  /**
   * What is running in this session. Read from the shell rather than from
   * the settings file, because the truth is a process and two files on
   * disk - a stored boolean would go on claiming the widget starts at
   * login after somebody deleted the unit by hand.
   */
  let services = $state<SessionStatus | null>(null);
  let serviceError = $state<string | null>(null);

  /**
   * The shortcut, as the daemon has it. The daemon is the only process
   * that can hear a key - `/dev/input` is root's - so this is asked rather
   * than stored, and it is null until the first answer or when there is no
   * daemon to ask.
   */
  let hotkey = $state<HotkeyStatus | null>(null);

  /** How long the learn window stays open. The daemon's own ceiling is 30s. */
  const LEARN_SECONDS = 10;

  /** Seconds left in an open learn window, or null when none is open. */
  let learning = $state<number | null>(null);
  /** The last learn window that closed with nothing pressed. */
  let learnTimedOut = $state(false);

  onMount(() => {
    if (!session.available()) return;
    void run(() => session.status());
    void refreshHotkey();
  });

  async function run(action: () => Promise<SessionStatus>) {
    try {
      services = await action();
      serviceError = null;
    } catch (e) {
      serviceError = String(e);
    }
  }

  /** Every hotkey call answers with the status, so they all land here. */
  async function runHotkey(action: () => Promise<HotkeyStatus>) {
    try {
      hotkey = await action();
      serviceError = null;
    } catch (e) {
      serviceError = String(e);
    }
  }

  async function refreshHotkey() {
    await runHotkey(() => daemon.hotkeyStatus());
  }

  /**
   * The one switch for the whole feature: the key the daemon acts on and
   * the widget that draws the result. They are one thing to use and so
   * they are one thing to turn off - a widget nothing can reach, or a key
   * that changes the mode with nothing on screen, are both half-broken
   * states nobody asks for.
   */
  const widgetOn = $derived((hotkey?.enabled ?? false) && (services?.osd.running ?? false));

  async function setWidget(on: boolean) {
    await runHotkey(() => daemon.setHotkeyEnabled(on));
    if (serviceError) return;
    await run(() => (on ? session.startOsd() : session.stopOsd()));
  }

  /**
   * Opens the learn window and counts it down on screen, because the whole
   * interaction is "press it now" and a button that just goes quiet for
   * ten seconds reads as broken.
   */
  async function captureShortcut() {
    learnTimedOut = false;
    learning = LEARN_SECONDS;
    const tick = setInterval(() => {
      if (learning !== null && learning > 0) learning -= 1;
    }, 1000);

    try {
      const result = await daemon.hotkeyLearn(LEARN_SECONDS * 1000);
      learnTimedOut = result.timedOut;
    } catch (e) {
      serviceError = String(e);
    } finally {
      clearInterval(tick);
      learning = null;
      await refreshHotkey();
    }
  }

  /** The file on disk is what decides; the setting mirrors it so the
   *  settings file does not disagree with the machine. */
  async function setAppAtLogin(enabled: boolean) {
    await run(() => session.setAppAtLogin(enabled));
    if (!serviceError) settings.set("autostart", enabled);
  }

  const widgetState = $derived(
    !services
      ? ""
      : !services.osd.binary
        ? t("settings.widgetMissing")
        : services.osd.running
          ? t("settings.widgetRunning")
          : t("settings.widgetStopped"),
  );

  function setPollInterval(seconds: number) {
    settings.set("pollIntervalMs", seconds * 1000);
    telemetry.restart();
  }

  const outcome = $derived(settings.outcome);

  // The app's own file path isn't reported until a load has happened, so
  // fall back to the conventional location rather than showing nothing.
  const configPath = $derived(
    settings.configPath ?? "~/.config/pyren/app.json",
  );
</script>

<div class="settings">
  <h1 class="page-title">{t("settings.title")}</h1>

  <Panel title={t("settings.language")}>
    <div class="row">
      <label for="main-lang">{t("settings.mainLanguage")}</label>
      <select
        id="main-lang"
        value={settings.current.mainLanguage}
        onchange={(e) => settings.set("mainLanguage", e.currentTarget.value)}
      >
        {#each availableLocales as code (code)}
          <option value={code}>{localeName(code)} ({code})</option>
        {/each}
      </select>
    </div>

    <div class="row">
      <label for="fallback-lang">{t("settings.fallbackLanguage")}</label>
      <select
        id="fallback-lang"
        value={settings.current.fallbackLanguage}
        onchange={(e) => settings.set("fallbackLanguage", e.currentTarget.value)}
      >
        {#each availableLocales as code (code)}
          <option value={code}>{localeName(code)} ({code})</option>
        {/each}
      </select>
    </div>
    <p class="hint">{@html t("help.translationsBody")}</p>
  </Panel>

  <!-- Permissions are not a preference - they are a one-time system change
       that needs authentication - so the controls live on the hardware
       check page. But this is where people come looking for them, and a
       setting you cannot find may as well not exist. -->
  <Panel title={t("admin.title")}>
    <p class="hint">{t("settings.permissionsHint")}</p>
    <a class="config-link" href="/drivers">{t("settings.permissionsLink")}</a>
  </Panel>

  <!-- Where settings live, and anything that went wrong reading them.
       A silent reset to defaults is exactly the kind of thing users are
       left guessing about, so it is stated here. -->
  <Panel title={t("settings.storage")}>
    {#if outcome?.status === "recovered"}
      <p class="notice err">
        {t("settings.configRecovered", { backup: outcome.backup ?? "?" })}
      </p>
    {:else if outcome?.status === "tooNew"}
      <p class="notice warn">{t("settings.configTooNew", { found: outcome.found })}</p>
    {/if}

    <div class="row">
      <span>{t("settings.configFile")}</span>
      <code>{configPath}</code>
    </div>

    {#if hardware.power}
      <div class="row">
        <span>{t("settings.daemonConfigFile")}</span>
        <code>{hardware.power.configPath}</code>
      </div>

      {#if hardware.power.configSaveError}
        <p class="notice err">
          {t("settings.configSaveFailed", { error: hardware.power.configSaveError })}
        </p>
      {/if}

      <div class="row">
        <span>
          {t("settings.restoreOnStart")}
          <small class="hint-inline">{t("settings.restoreOnStartHint")}</small>
        </span>
        <Toggle
          checked={hardware.power.restoreModeOnStart}
          onchange={(v) => hardware.setRestoreOnStart(v)}
          ariaLabel={t("settings.restoreOnStart")}
        />
      </div>
    {/if}
  </Panel>

  <Panel title={t("settings.units")}>
    <div class="row">
      <span>{t("settings.temperatureUnit")}</span>
      <Toggle
        checked={settings.current.tempUnit === "c"}
        onchange={(v) => settings.set("tempUnit", v ? "c" : "f")}
        labelOff="°F"
        labelOn="°C"
        ariaLabel={t("settings.temperatureUnit")}
      />
    </div>
  </Panel>

  <Panel title={t("settings.startup")}>
    <div class="row">
      <span>{t("settings.autostart")}</span>
      <Toggle
        checked={services?.app.startsAtLogin ?? settings.current.autostart}
        onchange={(v) => void setAppAtLogin(v)}
        ariaLabel={t("settings.autostart")}
      />
    </div>
    <div class="row">
      <span>{t("settings.startMinimized")}</span>
      <Toggle
        checked={settings.current.startMinimized}
        onchange={(v) => settings.set("startMinimized", v)}
        ariaLabel={t("settings.startMinimized")}
      />
    </div>
  </Panel>

  {#if session.available()}
    <Panel title={t("settings.services")}>
      <p class="hint">{t("settings.servicesHint")}</p>

      <div class="row">
        <span>{t("settings.daemonService")}</span>
        <span class="state">
          {telemetry.demo ? t("settings.daemonStopped") : t("settings.daemonRunning")}
        </span>
      </div>

      <!-- One switch for the key and the widget together: see setWidget. -->
      <div class="row">
        <span>
          {t("settings.widget")}
          <small class="hint-inline">{t("settings.widgetHint")}</small>
        </span>
        <Toggle
          checked={widgetOn}
          disabled={!services?.osd.binary || !hotkey}
          onchange={(v) => void setWidget(v)}
          ariaLabel={t("settings.widget")}
        />
      </div>

      {#if !services?.osd.binary}
        <p class="notice warn">{t("settings.widgetMissing")}</p>
      {:else if !widgetOn}
        <p class="hint">{widgetState}</p>
      {/if}

      <!-- The shortcut. Its own row rather than a page: on the laptops
           this was written for the vendor key never reaches Linux at all,
           so choosing a replacement is the normal path, not the fallback. -->
      <div class="row">
        <span>
          {t("settings.shortcut")}
          <small class="hint-inline">{t("settings.shortcutHint")}</small>
        </span>
        <div class="shortcut">
          {#if learning !== null}
            <span class="state pressing">{t("settings.shortcutPress", { n: learning })}</span>
          {:else}
            <kbd>{hotkey?.label ?? t("settings.shortcutNone")}</kbd>
            <button class="action" onclick={() => void captureShortcut()}>
              {hotkey?.label ? t("settings.shortcutChange") : t("settings.shortcutSet")}
            </button>
            {#if hotkey?.label}
              <button class="action" onclick={() => void runHotkey(() => daemon.hotkeyClear())}>
                {t("common.remove")}
              </button>
            {/if}
          {/if}
        </div>
      </div>

      {#if learnTimedOut}
        <p class="notice warn">{t("settings.shortcutTimedOut")}</p>
      {/if}

      <!-- Why nothing happens, when nothing does: not root, no keyboard,
           or no key bound. The daemon composes the sentence; repeating the
           logic here would be a second place for it to go stale. -->
      {#if hotkey && !hotkey.watching}
        <p class="notice err">{hotkey.detail}</p>
      {/if}

      <div class="row">
        <span>
          {t("settings.widgetPreview")}
          <small class="hint-inline">{t("settings.widgetPreviewHint")}</small>
        </span>
        <button
          class="action"
          disabled={!services?.osd.binary}
          onclick={() => void run(() => session.showOsd())}
        >
          {t("settings.widgetPreviewShow")}
        </button>
      </div>

      <div class="row">
        <span>{t("settings.widgetAtLogin")}</span>
        <Toggle
          checked={services?.osd.startsAtLogin ?? false}
          disabled={!services?.osd.binary}
          onchange={(v) => void run(() => session.setOsdAtLogin(v))}
          ariaLabel={t("settings.widgetAtLogin")}
        />
      </div>

      {#if serviceError}
        <p class="notice err">{t("settings.serviceFailed", { error: serviceError })}</p>
      {/if}
    </Panel>
  {/if}

  <Panel title={t("settings.advanced")}>
    <div class="row">
      <span>{t("settings.pollInterval")}</span>
      <div class="slider">
        <Slider
          value={settings.current.pollIntervalMs / 1000}
          min={1}
          max={10}
          gradient={false}
          ariaLabel={t("settings.pollInterval")}
          onchange={setPollInterval}
        />
        <b>{t("settings.seconds", { n: settings.current.pollIntervalMs / 1000 })}</b>
      </div>
    </div>

    <div class="row">
      <span>{t("settings.demoMode")}</span>
      <Toggle
        checked={settings.current.demoData}
        onchange={(v) => settings.set("demoData", v)}
        ariaLabel={t("settings.demoMode")}
      />
    </div>

    <div class="row">
      <span>{t("notices.driverMissingTitle")}</span>
      <Toggle
        checked={!settings.current.hideDriverNotice}
        onchange={(v) => settings.set("hideDriverNotice", !v)}
        ariaLabel={t("notices.dontShowAgain")}
      />
    </div>

    <div class="row">
      <span>{t("settings.resetSettings")}</span>
      <button
        class="danger"
        onclick={() => {
          settings.reset();
          hardware.reset();
        }}
      >
        {t("common.reset")}
      </button>
    </div>
  </Panel>
</div>

<style>
  .shortcut {
    display: flex;
    align-items: center;
    gap: 10px;
  }

  kbd {
    padding: 4px 10px;
    border: 1px solid var(--accent-2);
    border-radius: 3px;
    font-family: inherit;
    font-size: 13px;
    letter-spacing: 0.03em;
    white-space: nowrap;
  }

  /* The learn window is the one moment this page asks for something back,
     so it is the one thing on it that moves. */
  .pressing {
    animation: pulse 1.2s ease-in-out infinite;
  }

  @keyframes pulse {
    50% {
      opacity: 0.45;
    }
  }

  .config-link {
    display: inline-block;
    padding: 7px 22px;
    border: 1px solid var(--accent-2);
    border-radius: 2px;
    font-size: 13px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    text-decoration: none;
  }

  .settings {
    flex: 1;
    overflow-y: auto;
    padding: 24px 30px 44px;
    display: flex;
    flex-direction: column;
    gap: 18px;
    max-width: 860px;
  }

  .page-title {
    font-size: 24px;
  }

  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 22px;
    padding: 10px 0;
    border-bottom: 1px solid var(--line-soft);
    font-size: 14px;
  }

  .row:last-child {
    border-bottom: none;
  }

  .slider {
    display: flex;
    align-items: center;
    gap: 14px;
    width: 260px;
  }

  select {
    min-width: 220px;
    padding: 7px 12px;
    background: #2a2a2e;
    color: var(--text);
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    font: inherit;
    font-size: 13px;
  }

  .hint {
    margin: 12px 0 0;
    color: var(--text-mute);
    font-size: 12px;
    line-height: 1.5;
  }

  .hint-inline {
    display: block;
    margin-top: 3px;
    color: var(--text-mute);
    font-size: 12px;
    max-width: 460px;
  }

  .notice {
    margin: 0 0 12px;
    font-size: 13px;
    line-height: 1.5;
  }

  .notice.err {
    color: var(--danger);
  }

  .notice.warn {
    color: var(--warn);
  }

  code {
    font-size: 12px;
    color: var(--text-dim);
    background: var(--bg-inset);
    padding: 3px 8px;
    border-radius: var(--radius-sm);
    user-select: text;
  }

  .state {
    font-size: 13px;
    color: var(--text-mute);
  }

  .action {
    padding: 7px 16px;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--text-dim);
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }

  .action:hover:not(:disabled) {
    background: var(--bg-card-hover);
    color: var(--text);
  }

  .action:disabled {
    opacity: 0.4;
  }

  .danger {
    padding: 7px 16px;
    border: 1px solid var(--danger);
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--danger);
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }

  .danger:hover {
    background: rgba(255, 71, 71, 0.12);
  }
</style>
