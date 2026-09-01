<script lang="ts">
  /**
   * Key mapping. Picking a key opens an action panel below the board,
   * exactly like the reference app; the remap capture reads the physical
   * key from a real keydown so the value stored is a KeyboardEvent `code`
   * the daemon can act on later.
   */
  import Icon from "$lib/components/Icon.svelte";
  import Keyboard from "$lib/components/Keyboard.svelte";
  import { t } from "$lib/i18n/index.svelte";

  type Action = "remap" | "shortcut" | "macro" | "media";

  let selected = $state<string | null>(null);
  let action = $state<Action | "">("");
  let mappings = $state<Record<string, string>>({});
  let capturing = $state(false);
  let testValue = $state("");

  function captureKey(event: KeyboardEvent) {
    if (!capturing || !selected) return;
    event.preventDefault();
    mappings = { ...mappings, [selected]: event.code };
    capturing = false;
  }

  function clearSelected() {
    if (!selected) return;
    const { [selected]: _removed, ...rest } = mappings;
    mappings = rest;
  }

  function clearAll() {
    mappings = {};
    selected = null;
    action = "";
  }
</script>

<svelte:window onkeydown={captureKey} />

<div class="keys">
  <header class="head">
    <span class="prompt">{t("keys.selectKey")}</span>
    <button class="clear" disabled={Object.keys(mappings).length === 0} onclick={clearAll}>
      {t("keys.clearAll")}
    </button>
  </header>

  <div class="board-area">
    <Keyboard
      {selected}
      mapped={mappings}
      onselect={(id) => {
        selected = id;
        capturing = false;
      }}
    />
  </div>

  <div class="action-bar">
    <span class="action-label">{t("keys.keyMapping")}</span>
    {#if selected}
      <select value={action} onchange={(e) => (action = e.currentTarget.value as Action)}>
        <option value="">{t("keys.selectAction")}</option>
        <option value="remap">{t("keys.remap")}</option>
        <option value="shortcut">{t("keys.shortcut")}</option>
        <option value="macro">{t("keys.macro")}</option>
        <option value="media">{t("keys.media")}</option>
      </select>
    {/if}
  </div>

  {#if !selected}
    <p class="empty">{t("keys.emptyHint")}</p>
  {:else if action === "remap"}
    <div class="detail">
      <div class="detail-main">
        <header class="detail-head">
          <span class="dim">{t("keys.selectedKey")}</span>
          <strong>{selected}</strong>
          <button class="icon" onclick={clearSelected} aria-label={t("common.cancel")}>
            <Icon name="trash" size={16} />
          </button>
        </header>

        <div class="remap">
          <span class="remap-label">{t("keys.remapTo")}</span>
          <button class="capture" class:capturing onclick={() => (capturing = true)}>
            {mappings[selected] ?? "…"}
          </button>
          {#if mappings[selected]}
            <Icon name="check" size={18} class="ok-check" />
          {/if}
          <p class="hint">{t("keys.pressKey")}</p>
        </div>
      </div>

      <aside class="test">
        <h2>{t("keys.testTitle")}</h2>
        <textarea placeholder={t("keys.testHint")} bind:value={testValue}></textarea>
      </aside>
    </div>
  {:else if action}
    <p class="empty">{t("common.comingSoon")}</p>
  {/if}
</div>

<style>
  .keys {
    display: flex;
    flex-direction: column;
    min-height: 100%;
    background: var(--omen-black);
  }

  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 20px;
    padding: 18px 26px 0;
  }

  .prompt {
    font-size: 15px;
  }

  .clear {
    padding: 9px 20px;
    border: none;
    border-radius: 2px;
    background: #f2f2f4;
    color: #17171a;
    font-size: 12px;
    font-weight: 700;
    letter-spacing: 0.05em;
    text-transform: uppercase;
  }

  .clear:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .board-area {
    display: flex;
    justify-content: center;
    padding: 22px 26px 30px;
  }

  .action-bar {
    display: flex;
    align-items: center;
    gap: 22px;
    padding: 14px 26px;
    background: #4a4a50;
  }

  .action-label {
    font-size: 14px;
  }

  select {
    min-width: 300px;
    padding: 7px 12px;
    background: #2a2a2e;
    color: var(--text);
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    font: inherit;
    font-size: 14px;
  }

  .empty {
    flex: 1;
    display: grid;
    place-items: center;
    margin: 0;
    padding: 60px 20px;
    color: var(--text-mute);
    font-style: italic;
    font-size: 15px;
  }

  .detail {
    flex: 1;
    display: grid;
    grid-template-columns: 1fr minmax(240px, 320px);
  }

  .detail-head {
    display: flex;
    align-items: center;
    gap: 14px;
    padding: 14px 26px;
    border-bottom: 1px solid var(--line-soft);
  }

  .dim {
    color: var(--text-dim);
    font-size: 14px;
  }

  .detail-head strong {
    font-size: 20px;
    font-weight: 400;
  }

  .icon {
    margin-left: auto;
    display: grid;
    place-items: center;
    width: 30px;
    height: 30px;
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--text-dim);
  }

  .remap {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 10px;
    padding: 70px 20px;
  }

  .remap-label {
    font-size: 11px;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--text-dim);
  }

  .capture {
    min-width: 210px;
    padding: 10px 14px;
    border: 1px solid var(--text);
    border-radius: 2px;
    background: #111114;
    font-size: 15px;
    text-align: center;
  }

  .capture.capturing {
    border-color: var(--accent-3);
    color: var(--accent-3);
  }

  .hint {
    margin: 4px 0 0;
    color: var(--text-dim);
    font-size: 13px;
  }

  .test {
    display: flex;
    flex-direction: column;
    gap: 14px;
    padding: 18px;
    border-left: 1px solid var(--line-soft);
    background: #111114;
  }

  .test h2 {
    font-size: 14px;
    font-weight: 400;
    color: var(--text-dim);
  }

  textarea {
    flex: 1;
    min-height: 150px;
    resize: none;
    border: 1px dashed var(--line);
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--text);
    font: inherit;
    padding: 12px;
    text-align: center;
    user-select: text;
  }
</style>
