<script lang="ts">
  /** The small circled "i" that opens an explanation popover on click. */
  import Icon from "./Icon.svelte";
  import type { Snippet } from "svelte";

  let { children, align = "center" }: { children: Snippet; align?: "left" | "center" | "right" } =
    $props();
  let open = $state(false);
  let root = $state<HTMLElement>();
  let dot = $state<HTMLButtonElement>();
  let popEl = $state<HTMLElement>();
  let pos = $state({ top: 0, left: 0, right: 0 });

  /** Anchor the popover to the dot in viewport coordinates (it lives on <body>). */
  function place() {
    if (!dot) return;
    const r = dot.getBoundingClientRect();
    pos = {
      top: r.bottom + 10,
      left: align === "center" ? r.left + r.width / 2 : r.left,
      right: window.innerWidth - r.right,
    };
  }

  /**
   * Reparent the popover to <body> so no ancestor's `overflow`/`transform`
   * can clip it — a plain z-index cannot escape a clipping container.
   */
  function portal(node: HTMLElement) {
    document.body.appendChild(node);
    return { destroy: () => node.remove() };
  }

  /** Dismiss the popover when the click (or Escape) lands outside it. */
  $effect(() => {
    if (!open) return;
    place();

    const onPointer = (e: PointerEvent) => {
      const target = e.target as Node;
      if (root?.contains(target) || popEl?.contains(target)) return;
      open = false;
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") open = false;
    };
    const reposition = () => place();
    // `capture` so it still fires if something inside stops propagation.
    document.addEventListener("pointerdown", onPointer, true);
    document.addEventListener("keydown", onKey);
    window.addEventListener("scroll", reposition, true);
    window.addEventListener("resize", reposition);
    return () => {
      document.removeEventListener("pointerdown", onPointer, true);
      document.removeEventListener("keydown", onKey);
      window.removeEventListener("scroll", reposition, true);
      window.removeEventListener("resize", reposition);
    };
  });
</script>

<span class="tip" bind:this={root}>
  <button
    class="dot"
    bind:this={dot}
    onclick={() => (open = !open)}
    aria-label="i"
    aria-expanded={open}
  >
    <Icon name="info" size={15} stroke={1.8} />
  </button>
  {#if open}
    <span
      class="pop {align}"
      bind:this={popEl}
      use:portal
      style="top: {pos.top}px; {align === 'right'
        ? `right: ${pos.right}px`
        : `left: ${pos.left}px`}"
    >
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
    /* Fixed + reparented to <body>: immune to any ancestor clipping. */
    position: fixed;
    z-index: 1000;
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
    translate: -50% 0;
  }

  .pop.center::before {
    left: calc(50% - 4.5px);
  }

  .pop.left::before {
    left: 6px;
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
