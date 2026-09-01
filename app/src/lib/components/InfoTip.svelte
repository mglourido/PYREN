<script lang="ts">
  /** The small circled "i" that opens an explanation popover on click. */
  import Icon from "./Icon.svelte";
  import type { Snippet } from "svelte";

  let { children, align = "left" }: { children: Snippet; align?: "left" | "right" } = $props();
  let open = $state(false);
</script>

<span class="tip">
  <button class="dot" onclick={() => (open = !open)} aria-label="i" aria-expanded={open}>
    <Icon name="info" size={15} stroke={1.8} />
  </button>
  {#if open}
    <span class="pop {align}">
      <button class="close" onclick={() => (open = false)} aria-label="×">
        <Icon name="close" size={13} />
      </button>
      {@render children()}
    </span>
  {/if}
</span>

<style>
  .tip {
    position: relative;
    display: inline-flex;
    vertical-align: middle;
  }

  .dot {
    display: grid;
    place-items: center;
    width: 20px;
    height: 20px;
    padding: 0;
    border: none;
    border-radius: 50%;
    background: #d9d9de;
    color: #1a1a1e;
  }

  .pop {
    position: absolute;
    top: calc(100% + 8px);
    z-index: 40;
    width: 340px;
    padding: 14px 32px 14px 16px;
    background: #f2f2f4;
    color: #17171a;
    border-radius: var(--radius-sm);
    box-shadow: var(--shadow);
    font-size: 13px;
    line-height: 1.45;
    text-transform: none;
    font-weight: 400;
    user-select: text;
  }

  .pop.left {
    left: 0;
  }

  .pop.right {
    right: 0;
  }

  .close {
    position: absolute;
    top: 6px;
    right: 6px;
    display: grid;
    place-items: center;
    width: 20px;
    height: 20px;
    border: none;
    background: transparent;
    color: #55555c;
  }
</style>
