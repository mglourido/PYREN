<script lang="ts">
  import { onMount } from "svelte";
  import Panel from "$lib/components/Panel.svelte";
  import Slider from "$lib/components/Slider.svelte";
  import Toggle from "$lib/components/Toggle.svelte";
  import { availableLocales, localeName, t, tm } from "$lib/i18n/index.svelte";
  import { settings } from "$lib/stores/settings.svelte";
  import { hardware } from "$lib/stores/hardware.svelte";
  import { telemetry } from "$lib/stores/telemetry.svelte";
  import { session, type SessionStatus } from "$lib/api/session";
  import { admin, type AdminStatus } from "$lib/api/admin";
  import { daemon, errorText, type HotkeyStatus } from "$lib/api/daemon";

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

  /**
   * The privileges the daemon needs, as the shell sees them. Separate from
   * `services` because it answers a different question — that one is about
   * this login session, this one is about the machine — and because it is
   * the only thing here whose fixes cost a password.
   */
  let privileges = $state<AdminStatus | null>(null);
  /** True while a polkit prompt is up, so the toggle cannot be double-fired. */
  let elevating = $state(false);

  onMount(() => {
    if (!session.available()) return;
    void run(() => session.status());
    void refreshPrivileges();
    void refreshHotkey();
  });

  async function refreshPrivileges() {
    try {
      privileges = await admin.status();
    } catch {
      // Not reported: every row that reads this already renders as
      // "unknown", and a settings page is not where a broken shell call
      // should become a red banner.
      privileges = null;
    }
  }

  /**
   * The one switch that makes the hardware work: the daemon as a system
   * service, the socket group, and `acpi_call` for the lightbar and the
   * cleaner. Turning it off only stops the service — see `Grant::DisableService`.
   *
   * The status is re-read from the machine afterwards rather than assumed
   * from the answer, because the user can dismiss the password dialog and
   * a toggle that stayed where they left it would be a lie.
   */
  async function setDaemonAtBoot(enabled: boolean) {
    elevating = true;
    try {
      const result = await admin.grant(enabled ? "enableAtBoot" : "disableService");
      if (!result.applied && !result.cancelled) serviceError = result.detail;
      else serviceError = null;
    } catch (e) {
      serviceError = errorText(e);
    } finally {
      elevating = false;
      await refreshPrivileges();
    }
  }

  async function run(action: () => Promise<SessionStatus>) {
    try {
      services = await action();
      serviceError = null;
    } catch (e) {
      serviceError = errorText(e);
    }
  }

  /** Every hotkey call answers with the status, so they all land here. */
  async function runHotkey(action: () => Promise<HotkeyStatus>) {
    try {
      hotkey = await action();
      serviceError = null;
    } catch (e) {
      serviceError = errorText(e);
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
      serviceError = errorText(e);
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

  /** The files on disk when we have read them, the stored setting until then. */
  const autostartOn = $derived(services?.app.startsAtLogin ?? settings.current.autostart);

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
    <!-- The machine first, then the session, then this window. The daemon
         is what makes the hardware answer at all, and it is the only row
         here that is not merely a convenience — so it goes at the top and
         says what it costs. -->
    {#if privileges}
      <div class="row">
        <span>
          {t("settings.daemonAtBoot")}
          <small class="hint-inline">{t("settings.daemonAtBootHint")}</small>
        </span>
        <Toggle
          checked={privileges.serviceEnabled}
          disabled={elevating || !privileges.canElevate || !privileges.daemonBinary}
          onchange={(v) => void setDaemonAtBoot(v)}
          ariaLabel={t("settings.daemonAtBoot")}
        />
      </div>

      {#if !privileges.canElevate}
        <p class="notice warn">{t("settings.needsPolkit")}</p>
      {:else if !privileges.daemonBinary}
        <p class="notice warn">{t("settings.noDaemonBinary")}</p>
      {:else if privileges.needsRelogin}
        <p class="notice warn">{t("admin.groupNeedsRelogin")}</p>
      {/if}
    {/if}

    {#if session.available()}
      <div class="row">
        <span>
          {t("settings.widgetAtLogin")}
          <small class="hint-inline">{t("settings.widgetAtLoginHint")}</small>
        </span>
        <Toggle
          checked={services?.osd.startsAtLogin ?? false}
          disabled={!services?.osd.binary}
          onchange={(v) => void run(() => session.setOsdAtLogin(v))}
          ariaLabel={t("settings.widgetAtLogin")}
        />
      </div>

      <!-- The widget starts on its own — its `.path` unit needs neither the
           app nor a session manager — but it reads and changes power modes
           over the daemon's socket. Enabled without the daemon it comes up
           at login with nothing to talk to, which is a worse failure than
           not starting, because it looks like it worked. -->
      {#if services?.osd.startsAtLogin && privileges && !privileges.serviceEnabled}
        <p class="notice warn">{t("settings.widgetNeedsDaemon")}</p>
      {/if}
    {/if}

    <div class="row">
      <span>{t("settings.autostart")}</span>
      <Toggle
        checked={autostartOn}
        onchange={(v) => void setAppAtLogin(v)}
        ariaLabel={t("settings.autostart")}
      />
    </div>
    <!-- Only the app needs saying now. The widget gets a `.path` unit that
         watches for the compositor's Wayland socket, so it comes up on any
         desktop; the app cannot use the same trick, because a path unit
         re-triggers when what it started stops, and quitting Pyren has to
         mean quit. -->
    {#if services && !services.loginWorks && autostartOn}
      <p class="notice warn">{t("settings.autostartUnmanaged")}</p>
      <code class="block">exec-once = {services.app.loginCommand}</code>
    {/if}

    <div class="row">
      <span>
        {t("settings.startMinimized")}
        <small class="hint-inline">{t("settings.startMinimizedHint")}</small>
      </span>
      <Toggle
        checked={settings.current.startMinimized}
        onchange={(v) => settings.set("startMinimized", v)}
        ariaLabel={t("settings.startMinimized")}
      />
    </div>

    <div class="row">
      <span>
        {t("settings.closeToTray")}
        <small class="hint-inline">{t("settings.closeToTrayHint")}</small>
      </span>
      <Toggle
        checked={settings.current.closeToTray}
        onchange={(v) => settings.set("closeToTray", v)}
        ariaLabel={t("settings.closeToTray")}
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
        <p class="notice err">{tm(hotkey.detail)}</p>
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

      <!-- "Widget at login" used to live here. It moved to Startup, next
           to the other three things that start on their own: two toggles
           writing the same unit would have been two answers to one
           question. This panel keeps what is running *now*. -->

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
    appearance: none;
    min-width: 220px;
    padding: 7px 30px 7px 12px;
    background-color: #2a2a2e;
    color: var(--text);
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    font: inherit;
    font-size: 13px;
    background-image: linear-gradient(45deg, transparent 50%, var(--text-dim) 50%),
      linear-gradient(135deg, var(--text-dim) 50%, transparent 50%);
    background-position:
      right 15px center,
      right 10px center;
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

  /* A line the user has to copy, so it gets room and does not wrap in the
     middle of a path. */
  code.block {
    display: block;
    margin: 0 0 12px;
    padding: 8px 10px;
    color: var(--text);
    overflow-x: auto;
    white-space: pre;
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
