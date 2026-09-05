<script lang="ts">
  /** The small circled "i" that opens an explanation popover on click. */
  import Icon from "./Icon.svelte";
  import type { Snippet } from "svelte";

  let { children, align = "center" }: { children: Snippet; align?: "left" | "center" | "right" } =
    $props();
  let open = $state(false);
  let root = $state<HTMLElement>();

  /** Dismiss the popover when the click (or Escape) lands outside it. */
  $effect(() => {
    if (!open) return;

    const onPointer = (e: PointerEvent) => {
      if (root && !root.contains(e.target as Node)) open = false;
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") open = false;
    };
    // `capture` so it still fires if something inside stops propagation.
    document.addEventListener("pointerdown", onPointer, true);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onPointer, true);
      document.removeEventListener("keydown", onKey);
    };
  });
</script>

<span class="tip" bind:this={root}>
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
    line-height: 0;
  }

  .dot {
    display: grid;
    place-items: center;
    width: 20px;
    height: 20px;
    padding: 0;
    border: none;
    border-radius: 50%;
    background: var(--bg-card-hover);
    color: var(--text-dim);
    transition:
      background-color 0.12s ease,
      color 0.12s ease;
  }

  .dot:hover,
  .dot[aria-expanded="true"] {
    background: var(--info);
    color: var(--text);
  }

  .pop {
    position: absolute;
    top: calc(100% + 10px);
    z-index: 40;
    width: max-content;
    max-width: min(340px, calc(100vw - 32px));
    padding: 14px 32px 14px 16px;
    background: var(--bg-card);
    color: var(--text);
    border: 1px solid var(--line-soft);
    border-radius: var(--radius);
    box-shadow: var(--shadow);
    font-size: 13px;
    line-height: 1.45;
    text-transform: none;
    font-weight: 400;
    user-select: text;
  }

  .pop::before {
    content: "";
    position: absolute;
    bottom: 100%;
    width: 9px;
    height: 9px;
    background: var(--bg-card);
    border-left: 1px solid var(--line-soft);
    border-top: 1px solid var(--line-soft);
    transform: rotate(45deg);
    translate: 0 5px;
  }

  .pop.center {
    left: 50%;
    translate: -50% 0;
  }

  .pop.center::before {
    left: calc(50% - 4.5px);
  }

  .pop.left {
    left: 0;
  }

  .pop.left::before {
    left: 6px;
  }

  .pop.right {
    right: 0;
  }

  .pop.right::before {
    right: 6px;
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
    border-radius: 4px;
    background: transparent;
    color: var(--text-mute);
    transition: background-color 0.12s ease;
  }

  .close:hover {
    background: var(--bg-card-hover);
    color: var(--text);
  }
</style>
