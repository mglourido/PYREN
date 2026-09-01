<script lang="ts">
  import type { Snippet } from "svelte";
  type Props = { title?: string; children: Snippet; header?: Snippet; padded?: boolean };
  let { title, children, header, padded = true }: Props = $props();
</script>

<section class="panel">
  {#if title || header}
    <div class="head">
      {#if title}<h2>{title}</h2>{/if}
      {#if header}{@render header()}{/if}
    </div>
  {/if}
  <div class="body" class:padded>{@render children()}</div>
</section>

<style>
  .panel {
    background: var(--bg-panel);
    border: 1px solid var(--line-soft);
    border-radius: var(--radius);
  }

  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
    padding: 14px 18px;
    border-bottom: 1px solid var(--line-soft);
  }

  h2 {
    font-size: 15px;
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }

  .body.padded {
    padding: 18px;
  }
</style>
