<script lang="ts">
  import Panel from "$lib/components/Panel.svelte";
  import Slider from "$lib/components/Slider.svelte";
  import Toggle from "$lib/components/Toggle.svelte";
  import { availableLocales, localeName, t } from "$lib/i18n/index.svelte";
  import { settings } from "$lib/stores/settings.svelte";
  import { hardware } from "$lib/stores/hardware.svelte";
  import { telemetry } from "$lib/stores/telemetry.svelte";

  function setPollInterval(seconds: number) {
    settings.set("pollIntervalMs", seconds * 1000);
    telemetry.restart();
  }

  const outcome = $derived(settings.outcome);

  // The app's own file path isn't reported until a load has happened, so
  // fall back to the conventional location rather than showing nothing.
  const configPath = $derived(
    settings.configPath ?? "~/.config/omen-hub/app.json",
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
    <div class="row">
      <span>{t("settings.autostart")}</span>
      <Toggle
        checked={settings.current.autostart}
        onchange={(v) => settings.set("autostart", v)}
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
