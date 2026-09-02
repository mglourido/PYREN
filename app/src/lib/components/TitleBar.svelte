<script lang="ts">
  /**
   * App chrome. The window itself keeps its native decorations for now
   * (tiling WMs on Linux handle those far better than a custom drag
   * region), so this is the in-app header only: brand, quick power-mode
   * profile picker and the shortcuts on the right.
   */
  import Icon from "./Icon.svelte";
  import { t } from "$lib/i18n/index.svelte";
  import { hardware, type PowerMode } from "$lib/stores/hardware.svelte";
  import { goto } from "$app/navigation";

  const modes: PowerMode[] = ["eco", "balanced", "performance", "unlimited"];
</script>

<header class="titlebar">
  <div class="brand">
    <img class="logo" src="/favicon.svg" alt="" aria-hidden="true" />
    <span class="name">{t("app.name")}</span>
  </div>

  <div class="right">
    <label class="profile">
      <span class="profile-label">{t("performance.powerMode")}</span>
      <select
        value={hardware.state.powerMode}
        onchange={(e) => hardware.setPowerMode(e.currentTarget.value as PowerMode)}
      >
        {#each modes as mode (mode)}
          <option value={mode}>{t(`performance.modes.${mode}`)}</option>
        {/each}
      </select>
    </label>

    <button class="icon-btn" onclick={() => goto("/settings")} title={t("settings.title")}>
      <Icon name="settings" size={18} />
    </button>
    <button class="icon-btn" onclick={() => goto("/help")} title={t("help.title")}>
      <Icon name="help" size={18} />
    </button>
  </div>
</header>

<style>
  .titlebar {
    height: var(--titlebar-h);
    flex: 0 0 auto;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
    padding: 0 14px;
    background: var(--bg-chrome);
    border-bottom: 1px solid var(--line-soft);
  }

  .brand {
    display: flex;
    align-items: center;
    gap: 10px;
  }

  /* The Pyren star, from the same app/static/favicon.svg the webview and
     the bundler load for the window and desktop icons. */
  .logo {
    width: 18px;
    height: 18px;
    display: block;
  }

  .name {
    font-size: 15px;
    font-weight: 600;
    letter-spacing: 0.02em;
  }

  .right {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .profile {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .profile-label {
    color: var(--text-dim);
    font-size: 12px;
  }

  select {
    appearance: none;
    background: var(--bg-card);
    color: var(--text);
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    padding: 5px 26px 5px 10px;
    font: inherit;
    font-size: 12px;
    cursor: pointer;
    background-image: linear-gradient(45deg, transparent 50%, var(--text-dim) 50%),
      linear-gradient(135deg, var(--text-dim) 50%, transparent 50%);
    background-position:
      right 13px center,
      right 8px center;
    background-size:
      5px 5px,
      5px 5px;
    background-repeat: no-repeat;
  }

  .icon-btn {
    display: grid;
    place-items: center;
    width: 30px;
    height: 30px;
    border: none;
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--text-dim);
  }

  .icon-btn:hover {
    background: var(--bg-card);
    color: var(--text);
  }
</style>
