<script lang="ts">
  import { onMount } from "svelte";
  import Panel from "$lib/components/Panel.svelte";
  import Slider from "$lib/components/Slider.svelte";
  import Toggle from "$lib/components/Toggle.svelte";
  import Segmented from "$lib/components/Segmented.svelte";
  import RichText from "$lib/components/RichText.svelte";
  import { availableLocales, localeName, t, tm } from "$lib/i18n/index.svelte";
  import { settings } from "$lib/stores/settings.svelte";
  import { THEME_CODES, type ThemeCode } from "$lib/styles/themes";
  import { hardware, type PowerMode } from "$lib/stores/hardware.svelte";
  import { telemetry } from "$lib/stores/telemetry.svelte";
  import { session, type SessionStatus } from "$lib/api/session";
  import { admin, type AdminStatus } from "$lib/api/admin";
  import {
    daemon,
    errorText,
    onDaemonEvent,
    type FanSensorFailureAction,
    type HotkeyStatus,
  } from "$lib/api/daemon";
  import { debugLog, type DebugLogStatus } from "$lib/api/debug";
  import { revealItemInDir } from "@tauri-apps/plugin-opener";

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

  /** The debug-logging toggle, as the daemon has it. */
  let debugStatus = $state<DebugLogStatus | null>(null);
  let debugStatusError = $state<string | null>(null);

  onMount(() => {
    if (!session.available()) return;
    void run(() => session.status());
    void refreshPrivileges();
    void refreshHotkey();
    void refreshDebugStatus();
    const stopDebugWatch = onDaemonEvent((event) => {
      if (event.topic === "debug.changed") void refreshDebugStatus();
    });
    return stopDebugWatch;
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

  async function refreshDebugStatus() {
    try {
      debugStatus = await debugLog.status();
      debugStatusError = null;
    } catch (e) {
      debugStatus = null;
      debugStatusError = errorText(e);
    }
  }

  async function setDebugLogging(enabled: boolean) {
    try {
      debugStatus = await debugLog.setEnabled(enabled);
      debugStatusError = null;
      debugLog.action("debugLoggingToggled", { enabled });
    } catch (e) {
      debugStatusError = errorText(e);
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

  function setProcessPollInterval(seconds: number) {
    settings.set("processPollIntervalMs", seconds * 1000);
    telemetry.restartProcesses();
  }

  /**
   * The supervisor's tuning, shown in the units a person thinks in:
   * minutes rather than seconds, percent per core rather than a load
   * ratio, and the temperature unit picked above. Each field is sent on
   * commit (blur or Enter), never per keystroke - every send is a write
   * to the daemon's config file.
   */
  type AutoField = "pause" | "battery" | "loadLow" | "loadHigh" | "tempHigh" | "tempLow";
  const fahrenheit = $derived(settings.current.tempUnit === "f");
  const toUnit = (c: number) => Math.round(fahrenheit ? c * 1.8 + 32 : c);
  const fromUnit = (v: number) => (fahrenheit ? (v - 32) / 1.8 : v);

  /** Min/max per field, in display units. */
  const autoLimits = $derived<Record<AutoField, [number, number]>>({
    pause: [0, 60],
    battery: [5, 50],
    loadLow: [5, 95],
    loadHigh: [10, 200],
    tempHigh: [toUnit(60), toUnit(100)],
    tempLow: [toUnit(50), toUnit(95)],
  });

  function autoValue(field: AutoField): number {
    const a = hardware.power!.auto;
    switch (field) {
      case "pause": return Math.round(a.manualOverrideSecs / 60);
      case "battery": return Math.round(a.batteryLowPercent);
      case "loadLow": return Math.round(a.loadLow * 100);
      case "loadHigh": return Math.round(a.loadHigh * 100);
      case "tempHigh": return toUnit(a.tempHighC);
      case "tempLow": return toUnit(a.tempLowC);
    }
  }

  /** The last refusal, shown under the fields until the next change. */
  let autoError = $state<string | null>(null);

  async function setAutoValue(field: AutoField, input: HTMLInputElement) {
    const [min, max] = autoLimits[field];
    const raw = Number(input.value);
    if (!Number.isFinite(raw) || input.value.trim() === "") {
      input.value = String(autoValue(field));
      return;
    }
    const v = Math.min(max, Math.max(min, raw));
    const change = {
      pause: { manualOverrideSecs: Math.round(v * 60) },
      battery: { batteryLowPercent: v },
      loadLow: { loadLow: v / 100 },
      loadHigh: { loadHigh: v / 100 },
      tempHigh: { tempHighC: fromUnit(v) },
      tempLow: { tempLowC: fromUnit(v) },
    }[field];
    autoError = await hardware.updateAuto(change);
    // Show what the daemon holds now: the clamped value if it took, the
    // old one if it was refused.
    input.value = String(autoValue(field));
  }

  async function resetAutoTuning() {
    autoError = await hardware.updateAuto({
      manualOverrideSecs: 600,
      batteryLowPercent: 25,
      loadLow: 0.3,
      loadHigh: 0.7,
      tempHighC: 85,
      tempLowC: 75,
    });
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

  <div class="settings-scroll">
  <Panel title={t("settings.language")}>
    <div class="row lang-row">
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

    <div class="row lang-row">
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
  </Panel>

  <Panel title={t("settings.appearance")}>
    <div class="row">
      <span>{t("settings.theme")}</span>
      <select
        value={settings.current.theme}
        onchange={(e) => settings.set("theme", e.currentTarget.value as ThemeCode)}
      >
        {#each THEME_CODES as code (code)}
          <option value={code}>{t(`settings.themeNames.${code}`)}</option>
        {/each}
      </select>
    </div>
  </Panel>

  <Panel title={t("settings.systemMonitor")}>
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
      <span>{t("settings.processPollInterval")}</span>
      <div class="slider">
        <Slider
          value={settings.current.processPollIntervalMs / 1000}
          min={2}
          max={30}
          gradient={false}
          ariaLabel={t("settings.processPollInterval")}
          onchange={setProcessPollInterval}
        />
        <b>{t("settings.seconds", { n: settings.current.processPollIntervalMs / 1000 })}</b>
      </div>
    </div>

    <div class="row">
      <span>{t("settings.gaugeAnimations")}</span>
      <Toggle
        checked={settings.current.gaugeAnimations}
        onchange={(v) => settings.set("gaugeAnimations", v)}
        ariaLabel={t("settings.gaugeAnimations")}
      />
    </div>
  </Panel>

  <!-- Where settings live, and anything that went wrong reading them.
       A silent reset to defaults is exactly the kind of thing users are
       left guessing about, so it is stated here. -->
  <Panel title={t("settings.storage")}>
    {#if outcome?.status === "recovered"}
      <p class="notice err"><RichText text={t("settings.configRecovered", { backup: outcome.backup ?? "?" })} /></p>
    {:else if outcome?.status === "tooNew"}
      <p class="notice warn"><RichText text={t("settings.configTooNew", { found: outcome.found })} /></p>
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
        <p class="notice err"><RichText text={t("settings.configSaveFailed", { error: hardware.power.configSaveError })} /></p>
      {/if}

      <div class="row">
        <span>
          {t("settings.restoreOnStart")}
          <small class="hint-inline"><RichText text={t("settings.restoreOnStartHint")} /></small>
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

  <!-- The power-mode supervisor, all in one place. The home screen keeps
       its own switches for the day-to-day ones; this is where the whole
       setup is, including what the home screen has no room for. The
       per-source rows are the same state as the home screen's, so the two
       can never disagree. -->
  {#if hardware.power}
    {@const auto = hardware.power.auto}
    <Panel title={t("settings.autoControl")}>
      <div class="row">
        <span>
          {t("settings.autoControlMaster")}
          <small class="hint-inline"><RichText text={t("settings.autoControlMasterHint")} /></small>
        </span>
        <Toggle
          checked={auto.enabled}
          onchange={(v) => void hardware.setAutoEnabled(v)}
          ariaLabel={t("settings.autoControlMaster")}
        />
      </div>

      <div class="row">
        <span>
          {t("home.autoEco")}
          <small class="hint-inline"><RichText text={t("home.autoEcoHint")} /></small>
        </span>
        <Toggle
          checked={hardware.state.autoEco}
          onchange={(v) => void hardware.setAutoSwitch(v, hardware.state.autoPerformance)}
          ariaLabel={t("home.autoEco")}
        />
      </div>

      <div class="row">
        <label for="prefer-battery">
          {t("home.preferOnBattery")}
          <small class="hint-inline"><RichText text={t("settings.preferOnBatteryHint")} /></small>
        </label>
        <select
          id="prefer-battery"
          value={auto.preferredOnBattery}
          onchange={(e) =>
            void hardware.setAutoPreference("battery", e.currentTarget.value as PowerMode)}
        >
          {#each ["eco", "balanced"] as id (id)}
            <option value={id}>{t(`performance.modes.${id}`)}</option>
          {/each}
        </select>
      </div>

      <div class="row">
        <span>
          {t("home.autoPerformance")}
          <small class="hint-inline"><RichText text={t("home.autoPerformanceHint")} /></small>
        </span>
        <Toggle
          checked={hardware.state.autoPerformance}
          onchange={(v) => void hardware.setAutoSwitch(hardware.state.autoEco, v)}
          ariaLabel={t("home.autoPerformance")}
        />
      </div>

      <div class="row">
        <label for="prefer-mains">
          {t("home.preferOnMains")}
          <small class="hint-inline"><RichText text={t("settings.preferOnMainsHint")} /></small>
        </label>
        <select
          id="prefer-mains"
          value={auto.preferredOnMains}
          onchange={(e) =>
            void hardware.setAutoPreference("mains", e.currentTarget.value as PowerMode)}
        >
          {#each ["balanced", "performance"] as id (id)}
            <option value={id}>{t(`performance.modes.${id}`)}</option>
          {/each}
        </select>
      </div>

      <!-- As on the home screen: no sensor, no switch. -->
      {#if hardware.power.thermal.available}
        <div class="row">
          <span>
            {t("home.thermalBackOff")}
            <small class="hint-inline"><RichText text={t("home.thermalBackOffHint")} /></small>
          </span>
          <Toggle
            checked={auto.backOffWhenHot}
            onchange={(v) => void hardware.setThermalBackOff(v)}
            ariaLabel={t("home.thermalBackOff")}
          />
        </div>
      {/if}

      <!-- What the home screen has no room for. Folded away because the
           defaults are right for nearly everyone, and a wall of numbers
           next to the switches would suggest they need touching. -->
      <details class="advanced">
        <summary>{t("settings.autoAdvanced")}</summary>

        {#snippet field(id: AutoField, label: string, hint: string, unit: string)}
          <div class="row">
            <label for="auto-{id}">
              {label}
              <small class="hint-inline"><RichText text={hint} /></small>
            </label>
            <span class="number">
              <input
                id="auto-{id}"
                type="number"
                min={autoLimits[id][0]}
                max={autoLimits[id][1]}
                step="1"
                value={autoValue(id)}
                onchange={(e) => void setAutoValue(id, e.currentTarget)}
              />
              <span class="unit">{unit}</span>
            </span>
          </div>
        {/snippet}

        {@render field("pause", t("settings.autoPause"), t("settings.autoPauseHint"), t("settings.unitMinutes"))}
        {@render field("battery", t("settings.autoBatteryLow"), t("settings.autoBatteryLowHint"), "%")}
        {@render field("loadLow", t("settings.autoLoadLow"), t("settings.autoLoadLowHint"), t("settings.unitPerCore"))}
        {@render field("loadHigh", t("settings.autoLoadHigh"), t("settings.autoLoadHighHint"), t("settings.unitPerCore"))}
        {#if hardware.power.thermal.available}
          {@render field("tempHigh", t("settings.autoTempHigh"), t("settings.autoTempHighHint"), fahrenheit ? "°F" : "°C")}
          {@render field("tempLow", t("settings.autoTempLow"), t("settings.autoTempLowHint"), fahrenheit ? "°F" : "°C")}
        {/if}

        {#if autoError}
          <p class="notice err">{autoError}</p>
        {/if}

        <div class="row">
          <span>{t("settings.autoAdvancedReset")}</span>
          <button class="action" onclick={() => void resetAutoTuning()}>
            {t("common.reset")}
          </button>
        </div>
      </details>

      {#if !auto.enabled}
        <p class="hint">{t("settings.autoControlOff")}</p>
      {:else if hardware.power.autoManualBaseline && hardware.power.autoManualBaseline !== "unlimited"}
        <p class="hint">
          {t("home.followingManual", {
            mode: t(`performance.modes.${hardware.power.autoManualBaseline}`),
          })}
        </p>
      {/if}
    </Panel>
  {/if}

  <!-- Fan control settings. The keep-mode row needs only mode switching;
       the floor rows below need a commandable speed, so on a machine
       limited to auto and max there is no floor to choose. -->
  {#if hardware.fan?.capabilities.switchMode}
    {@const fan = hardware.fan}
    <Panel title={t("settings.fans")}>
      <div class="row">
        <span>
          {t("settings.keepFanMode")}
          <small class="hint-inline"><RichText text={t("settings.keepFanModeHint")} /></small>
        </span>
        <Toggle
          checked={fan.restoreModeOnStart}
          onchange={(v) => void hardware.setFanRestoreOnStart(v)}
          ariaLabel={t("settings.keepFanMode")}
        />
      </div>
      <div class="row">
        <span>
          {t("settings.thermalSafetyChecker")}
          <small class="hint-inline"><RichText text={t("settings.thermalSafetyCheckerHint", {
              hot: `${toUnit(fan.safety?.hotC ?? 85)} ${fahrenheit ? "°F" : "°C"}`,
              cool: `${toUnit(fan.safety?.coolC ?? 75)} ${fahrenheit ? "°F" : "°C"}`,
            })} /></small>
        </span>
        <Toggle
          checked={fan.thermalSafetyChecker}
          onchange={(v) => void hardware.setThermalSafetyChecker(v)}
          ariaLabel={t("settings.thermalSafetyChecker")}
        />
      </div>
      <div class="row">
        <span>
          {t("settings.sensorFailureAction")}
          <small class="hint-inline"><RichText text={t("settings.sensorFailureActionHint")} /></small>
        </span>
        <Segmented
          variant="pill"
          value={fan.sensorFailureAction ?? "max"}
          options={[
            { value: "max", label: t("settings.sensorFailureMax") },
            { value: "auto", label: t("settings.sensorFailureAuto") },
          ]}
          onchange={(v) => void hardware.setSensorFailureAction(v as FanSensorFailureAction)}
        />
      </div>
      <div class="row">
        <span>
          {t("settings.perFanRpm")}
          <small class="hint-inline"><RichText text={t("settings.perFanRpmHint")} /></small>
        </span>
        <Toggle
          checked={settings.current.perFanRpm}
          onchange={(v) => settings.set("perFanRpm", v)}
          ariaLabel={t("settings.perFanRpm")}
        />
      </div>
      {#if fan.capabilities.setSpeed}
      <div class="row">
        <span>
          {t("settings.keepDriverFloor")}
          <small class="hint-inline"><RichText text={t("settings.keepDriverFloorHint", {
              driver: fan.driverMinRpm !== null ? String(fan.driverMinRpm) : "?",
              pyren: fan.pyrenMinRpm !== null ? String(fan.pyrenMinRpm) : "?",
            })} /></small>
        </span>
        <!-- Turning it off needs both a driver that can be told another
             floor and a floor to tell it; turning it back on never does. -->
        <Toggle
          checked={fan.keepDriverFloor}
          disabled={fan.keepDriverFloor && (!fan.floorOverrideSupported || fan.pyrenMinRpm === null)}
          onchange={(v) => void hardware.setKeepDriverFloor(v)}
          ariaLabel={t("settings.keepDriverFloor")}
        />
      </div>
      {#if !fan.floorOverrideSupported}
        <p class="notice warn"><RichText text={t("settings.floorNeedsDriver")} /></p>
      {:else if fan.pyrenMinRpm === null}
        <p class="notice warn"><RichText text={t("settings.floorNotMeasured")} /></p>
      {/if}
      {/if}
    </Panel>
  {/if}

  <Panel title={t("settings.debugLogs")}>
    <div class="row">
      <span>
        {t("settings.debugLogsEnable")}
        <small class="hint-inline"><RichText text={t("settings.debugLogsEnableHint")} /></small>
      </span>
      <Toggle
        checked={debugStatus?.enabled ?? false}
        onchange={(v) => void setDebugLogging(v)}
        ariaLabel={t("settings.debugLogsEnable")}
      />
    </div>
    {#if debugStatusError}
      <p class="notice warn">{debugStatusError}</p>
    {:else if debugStatus && !debugStatus.daemonDirWritable}
      <p class="notice warn">
        {t("settings.debugLogsNotWritable", { path: debugStatus.daemonDir })}
      </p>
    {/if}
    {#if debugStatus}
      <div class="row">
        <span>{t("settings.debugLogsFolder")}</span>
        <button
          type="button"
          class="action"
          onclick={() => void revealItemInDir(debugStatus!.userDir)}
        >
          {t("settings.debugLogsOpenFolder")}
        </button>
      </div>
      {#if debugStatus.daemonDir !== debugStatus.userDir && debugStatus.daemonDirWritable}
        <div class="row">
          <span>{t("settings.debugLogsFolder")}</span>
          <button
            type="button"
            class="action"
            onclick={() => void revealItemInDir(debugStatus!.daemonDir)}
          >
            {t("settings.debugLogsOpenDaemonFolder")}
          </button>
        </div>
      {/if}
    {/if}
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
          <small class="hint-inline"><RichText text={t("settings.daemonAtBootHint")} /></small>
        </span>
        <Toggle
          checked={privileges.serviceEnabled}
          disabled={elevating || !privileges.canElevate || !privileges.daemonBinary}
          onchange={(v) => void setDaemonAtBoot(v)}
          ariaLabel={t("settings.daemonAtBoot")}
        />
      </div>

      {#if !privileges.canElevate}
        <p class="notice warn"><RichText text={t("settings.needsPolkit")} /></p>
      {:else if !privileges.daemonBinary}
        <p class="notice warn"><RichText text={t("settings.noDaemonBinary")} /></p>
      {:else if privileges.needsRelogin}
        <p class="notice warn"><RichText text={t("admin.groupNeedsRelogin")} /></p>
      {/if}
    {/if}

    {#if session.available()}
      <div class="row">
        <span>
          {t("settings.widgetAtLogin")}
          <small class="hint-inline"><RichText text={t("settings.widgetAtLoginHint")} /></small>
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
        <p class="notice warn"><RichText text={t("settings.widgetNeedsDaemon")} /></p>
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
      <p class="notice warn"><RichText text={t("settings.autostartUnmanaged")} /></p>
      <code class="block">exec-once = {services.app.loginCommand}</code>
    {/if}

    <div class="row">
      <span>
        {t("settings.startMinimized")}
        <small class="hint-inline"><RichText text={t("settings.startMinimizedHint")} /></small>
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
        <small class="hint-inline"><RichText text={t("settings.closeToTrayHint")} /></small>
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
      <p class="hint"><RichText text={t("settings.servicesHint")} /></p>

      <!-- One switch for the key and the widget together: see setWidget. -->
      <div class="row">
        <span>
          {t("settings.widget")}
          <small class="hint-inline"><RichText text={t("settings.widgetHint")} /></small>
        </span>
        <Toggle
          checked={widgetOn}
          disabled={!services?.osd.binary || !hotkey}
          onchange={(v) => void setWidget(v)}
          ariaLabel={t("settings.widget")}
        />
      </div>

      {#if !services?.osd.binary}
        <p class="notice warn"><RichText text={t("settings.widgetMissing")} /></p>
      {:else if !widgetOn}
        <p class="hint">{widgetState}</p>
      {/if}

      <!-- The shortcut. Its own row rather than a page: on the laptops
           this was written for the vendor key never reaches Linux at all,
           so choosing a replacement is the normal path, not the fallback. -->
      <div class="row">
        <span>
          {t("settings.shortcut")}
          <small class="hint-inline"><RichText text={t("settings.shortcutHint")} /></small>
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
        <p class="notice warn"><RichText text={t("settings.shortcutTimedOut")} /></p>
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
          <small class="hint-inline"><RichText text={t("settings.widgetPreviewHint")} /></small>
        </span>
        <button
          class="action"
          disabled={!services?.osd.binary}
          onclick={() => void run(() => session.showOsd())}
        >
          {t("settings.widgetPreviewShow")}
        </button>
      </div>

      <!-- The fan-mode row in the widget. Only where the machine can switch
           fan modes at all - and the widget reads this straight out of
           app.json, so it takes hold the next time the widget opens. -->
      {#if hardware.fan?.capabilities.switchMode}
        <div class="row">
          <span>
            {t("settings.widgetFanModes")}
            <small class="hint-inline"><RichText text={t("settings.widgetFanModesHint")} /></small>
          </span>
          <Toggle
            checked={settings.current.widgetFanModes}
            onchange={(v) => {
              settings.set("widgetFanModes", v);
              // Turning the fan row off can't leave the widget empty:
              // the power row comes back.
              if (!v) settings.set("widgetPowerModes", true);
            }}
            ariaLabel={t("settings.widgetFanModes")}
          />
        </div>

        <!-- Only offered while the fan row is on, so there is always at
             least one row to show. -->
        {#if settings.current.widgetFanModes}
          <div class="row">
            <span>
              {t("settings.widgetPowerModes")}
              <small class="hint-inline">
                <RichText text={t("settings.widgetPowerModesHint")} />
              </small>
            </span>
            <Toggle
              checked={settings.current.widgetPowerModes}
              onchange={(v) => settings.set("widgetPowerModes", v)}
              ariaLabel={t("settings.widgetPowerModes")}
            />
          </div>
        {/if}
      {/if}

      <!-- "Widget at login" used to live here. It moved to Startup, next
           to the other three things that start on their own: two toggles
           writing the same unit would have been two answers to one
           question. This panel keeps what is running *now*. -->

      {#if serviceError}
        <p class="notice err"><RichText text={t("settings.serviceFailed", { error: serviceError })} /></p>
      {/if}
    </Panel>
  {/if}

  <Panel title={t("settings.updates")}>
    <div class="row">
      <span>{t("settings.autoCheckUpdates")}</span>
      <Toggle
        checked={settings.current.autoCheckUpdates}
        onchange={(v) => settings.set("autoCheckUpdates", v)}
        ariaLabel={t("settings.autoCheckUpdates")}
      />
    </div>

    <div class="row">
      <span>{t("settings.notifyUpdateOnce")}</span>
      <Toggle
        checked={settings.current.notifyUpdateOnce}
        onchange={(v) => settings.set("notifyUpdateOnce", v)}
        ariaLabel={t("settings.notifyUpdateOnce")}
      />
    </div>
  </Panel>

  <Panel title={t("settings.advanced")}>
    <div class="row">
      <span>{t("settings.driverNotice")}</span>
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
</div>

<style>
  .advanced {
    border-bottom: 1px solid var(--line-soft);
  }

  .advanced summary {
    padding: 10px 0;
    font-size: 14px;
    color: var(--text-dim);
    cursor: pointer;
  }

  .advanced .row {
    padding-left: 14px;
  }

  .number {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-shrink: 0;
  }

  .number input {
    width: 76px;
    padding: 6px 8px;
    background: var(--bg-card);
    color: var(--text);
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    font: inherit;
    font-size: 13px;
    text-align: right;
  }

  .unit {
    min-width: 64px;
    color: var(--text-mute);
    font-size: 12px;
  }

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
    min-height: 0;
    overflow: hidden;
    padding: 0 30px 32px;
    display: flex;
    flex-direction: column;
    width: 100%;
    max-width: 990px;
  }

  .settings-scroll {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding-right: 6px;
    display: flex;
    flex-direction: column;
    gap: 18px;
  }

  .page-title {
    font-size: 24px;
    flex: 0 0 auto;
    margin: 0;
    padding: 24px 0 18px;
  }

  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    flex-wrap: wrap;
    gap: 10px 22px;
    padding: 10px 0;
    border-bottom: 1px solid var(--line-soft);
    font-size: 14px;
  }

  .row:last-child {
    border-bottom: none;
  }

  .lang-row {
    padding: 6px 0;
  }

  .slider {
    display: flex;
    align-items: center;
    gap: 14px;
    width: 260px;
  }

  .slider b {
    white-space: nowrap;
    flex-shrink: 0;
  }

  select {
    appearance: none;
    min-width: 220px;
    max-width: 100%;
    margin: 4px 0;
    padding: 7px 30px 7px 12px;
    background-color: var(--bg-card);
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
    background: var(--bg-card);
    color: var(--text);
  }

  .hint {
    margin: 0;
    padding: 10px 0;
    color: var(--text-mute);
    font-size: 12px;
    line-height: 1.5;
    border-bottom: 1px solid var(--line-soft);
  }

  .hint:last-child {
    border-bottom: none;
  }

  .hint-inline {
    display: block;
    margin-top: 3px;
    color: var(--text-mute);
    font-size: 12px;
    max-width: 529px;
  }

  .notice {
    margin: 0 0 12px;
    font-size: 13px;
    line-height: 1.5;
  }

  /* Technical terms picked out by RichText's **bold** spans: lift them
     back to full-strength text so they stand out of the muted hint. */
  .hint :global(strong),
  .hint-inline :global(strong) {
    color: var(--text-dim);
    font-weight: 600;
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
