<script lang="ts">
  /**
   * The notification history, as a box in the middle of the app rather
   * than a dropdown pinned to the header bell - a centred dialog is easier
   * to read and to dismiss, and does not have to dodge the window edge.
   *
   * Backdrop pattern borrowed from `InstallProgress.svelte`, but this one
   * dismisses: click-away, Esc, and a close button. Opening it already
   * marked everything read (see `notifications.toggle`), so there is no
   * per-row read control - the list just stops being bold.
   */
  import Icon from "./Icon.svelte";
  import { t } from "$lib/i18n/index.svelte";
  import { notifications, timeAgo } from "$lib/stores/notifications.svelte";
  import { goto } from "$app/navigation";

  const list = $derived(notifications.list);

  function onKey(event: KeyboardEvent) {
    if (event.key === "Escape") notifications.close();
  }

  function recalibrate() {
    notifications.close();
    void goto("/drivers");
  }
</script>

<svelte:window onkeydown={onKey} />

{#if notifications.open}
  <div class="backdrop">
    <!-- A real button so click-away is keyboard-reachable too; the panel
         is a sibling stacked on top, so a click on it never reaches here. -->
    <button class="scrim" aria-label={t("common.close")} onclick={() => notifications.close()}></button>

    <div class="panel" role="dialog" aria-modal="true" aria-label={t("notifications.title")}>
      <header>
        <h2>{t("notifications.title")}</h2>
        <div class="tools">
          {#if list.length > 0}
            <button class="link" onclick={() => notifications.clear()}>
              {t("notifications.clear")}
            </button>
          {/if}
          <button class="close" onclick={() => notifications.close()} aria-label={t("common.close")}>
            <Icon name="close" size={16} />
          </button>
        </div>
      </header>

      {#if list.length === 0}
        <p class="empty">
          <Icon name="bell" size={22} />
          {t("notifications.empty")}
        </p>
      {:else}
        <ul class="items">
          {#each list as n (n.id)}
            <li class="item {n.kind}" class:unread={!n.read}>
              <span class="dot" aria-hidden="true"></span>
              <Icon name={n.icon} size={18} class="glyph" />
              <div class="text selectable">
                <div class="row">
                  <span class="title">{n.title}</span>
                  <span class="age">{timeAgo(n.at)}</span>
                </div>
                <p class="body">{n.body}</p>
                {#if n.action === "recalibrate"}
                  <button class="link" onclick={recalibrate}>
                    {t("notifications.recalibrate")}
                  </button>
                {/if}
              </div>
            </li>
          {/each}
        </ul>
      {/if}
    </div>
  </div>
{/if}

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    z-index: 200;
    display: grid;
    place-items: center;
    padding: 24px;
  }

  .scrim {
    position: absolute;
    inset: 0;
    border: none;
    padding: 0;
    background: rgba(0, 0, 0, 0.72);
    backdrop-filter: blur(3px);
    cursor: default;
  }

  .panel {
    position: relative;
    width: min(520px, 100%);
    max-height: 100%;
    display: flex;
    flex-direction: column;
    gap: 14px;
    padding: 20px 22px;
    border: 1px solid var(--line);
    border-radius: var(--radius-lg);
    background: var(--bg-panel);
    box-shadow: var(--shadow);
  }

  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
  }

  h2 {
    margin: 0;
    font-size: 1.05rem;
    font-weight: 600;
  }

  .tools {
    display: flex;
    align-items: center;
    gap: 10px;
  }

  .link {
    border: none;
    background: transparent;
    color: var(--text-dim);
    text-decoration: underline;
    padding: 0;
    font: inherit;
    font-size: 12px;
  }

  .link:hover {
    color: var(--text);
  }

  .close {
    display: grid;
    place-items: center;
    width: 26px;
    height: 26px;
    border: none;
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--text-dim);
  }

  .close:hover {
    background: var(--bg-card);
    color: var(--text);
  }

  .empty {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 10px;
    margin: 0;
    padding: 28px 0;
    color: var(--text-mute);
    font-size: 13px;
  }

  .items {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
    overflow-y: auto;
    max-height: 60vh;
  }

  .item {
    display: grid;
    grid-template-columns: 8px auto 1fr;
    align-items: start;
    gap: 10px;
    padding: 12px;
    border: 1px solid var(--line-soft);
    border-radius: var(--radius);
    background: var(--bg-card);
  }

  .item .dot {
    width: 6px;
    height: 6px;
    margin-top: 6px;
    border-radius: var(--radius-pill);
    background: transparent;
  }

  .item.unread .dot {
    background: var(--accent-2);
  }

  .item.unread .title {
    font-weight: 600;
  }

  .item :global(.glyph) {
    margin-top: 1px;
    color: var(--text-dim);
  }

  .item.warning :global(.glyph) {
    color: var(--warn);
  }

  .text {
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 3px;
  }

  .row {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 10px;
  }

  .title {
    font-size: 13px;
  }

  .age {
    flex: 0 0 auto;
    color: var(--text-mute);
    font-size: 11px;
  }

  .body {
    margin: 0;
    color: var(--text-dim);
    font-size: 12px;
    line-height: 1.5;
  }

  .text .link {
    align-self: flex-start;
    margin-top: 2px;
  }
</style>
