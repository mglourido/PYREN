<script lang="ts">
  /**
   * Full-width notice strip. `warning` reproduces the amber "performance
   * settings are overridden on battery" bar; `info` the blue one below it.
   */
  import type { Snippet } from "svelte";
  import Icon from "./Icon.svelte";

  type Props = {
    kind?: "warning" | "info" | "danger";
    title?: string;
    dismissible?: boolean;
    ondismiss?: () => void;
    children: Snippet;
    actions?: Snippet;
  };
  let {
    kind = "warning",
    title,
    dismissible = false,
    ondismiss,
    children,
    actions,
  }: Props = $props();
</script>

<div class="banner {kind}">
  <Icon name={kind === "info" ? "diamond" : "warning"} size={20} />
  {#if title}<strong class="title">{title}</strong>{/if}
  <p class="body">{@render children()}</p>
  {#if actions}
    <div class="actions">{@render actions()}</div>
  {/if}
  {#if dismissible}
    <button class="close" onclick={() => ondismiss?.()} aria-label="×">
      <Icon name="close" size={16} />
    </button>
  {/if}
</div>

<style>
  .banner {
    display: flex;
    align-items: center;
    gap: 14px;
    padding: 10px 16px;
    font-size: 13px;
    line-height: 1.4;
    border-left: 3px solid;
  }

  /* A faint wash of the status colour over the panel surface, so the tint
     reads on both the dark and the light theme. */
  .warning {
    background: color-mix(in srgb, var(--warn) 14%, var(--bg-panel));
    border-color: var(--warn);
  }

  .info {
    background: color-mix(in srgb, var(--info) 14%, var(--bg-panel));
    border-color: var(--info);
  }

  .danger {
    background: color-mix(in srgb, var(--danger) 14%, var(--bg-panel));
    border-color: var(--danger);
  }

  .title {
    flex: 0 0 auto;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    font-size: 13px;
  }

  .body {
    flex: 1;
    margin: 0;
    color: var(--text);
  }

  .actions {
    flex: 0 0 auto;
    display: flex;
    align-items: center;
    gap: 12px;
  }

  .close {
    flex: 0 0 auto;
    display: grid;
    place-items: center;
    width: 26px;
    height: 26px;
    border: none;
    background: transparent;
    color: inherit;
    opacity: 0.75;
  }

  .close:hover {
    opacity: 1;
  }
</style>
