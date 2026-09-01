<script lang="ts">
  /**
   * Horizontal tab strip for the device pages. Like the reference app, the
   * strip scrolls when the tabs don't fit and shows chevrons at the edges
   * instead of wrapping to a second row.
   */
  import { page } from "$app/state";
  import Icon from "./Icon.svelte";
  import { t } from "$lib/i18n/index.svelte";
  import { DEVICE_TABS } from "$lib/nav";

  let strip = $state<HTMLDivElement | null>(null);
  let canLeft = $state(false);
  let canRight = $state(false);

  function measure() {
    if (!strip) return;
    canLeft = strip.scrollLeft > 4;
    canRight = strip.scrollLeft + strip.clientWidth < strip.scrollWidth - 4;
  }

  function scrollBy(direction: -1 | 1) {
    strip?.scrollBy({ left: direction * 260, behavior: "smooth" });
  }

  $effect(() => {
    measure();
    const observer = new ResizeObserver(measure);
    if (strip) observer.observe(strip);
    return () => observer.disconnect();
  });
</script>

<div class="tabbar">
  <button class="arrow" class:hidden={!canLeft} onclick={() => scrollBy(-1)} aria-label="←">
    <Icon name="chevronLeft" size={20} />
  </button>

  <div class="strip" bind:this={strip} onscroll={measure}>
    {#each DEVICE_TABS as tab (tab.href)}
      <a href={tab.href} class="tab" class:active={page.url.pathname === tab.href}>
        {t(tab.label)}
      </a>
    {/each}
  </div>

  <button class="arrow" class:hidden={!canRight} onclick={() => scrollBy(1)} aria-label="→">
    <Icon name="chevronRight" size={20} />
  </button>
</div>

<style>
  .tabbar {
    flex: 0 0 auto;
    display: flex;
    align-items: stretch;
    height: var(--tabbar-h);
    background: var(--bg-window);
    border-bottom: 1px solid var(--line-soft);
    padding: 8px 4px 0;
  }

  .strip {
    flex: 1;
    display: flex;
    gap: 4px;
    overflow-x: auto;
    scrollbar-width: none;
  }

  .strip::-webkit-scrollbar {
    display: none;
  }

  .tab {
    position: relative;
    flex: 0 0 auto;
    display: grid;
    place-items: center;
    padding: 0 22px;
    background: #101013;
    border: 1px solid var(--line-soft);
    border-bottom: none;
    border-radius: 4px 4px 0 0;
    color: var(--text);
    text-decoration: none;
    font-size: 14px;
    white-space: nowrap;
  }

  .tab:hover {
    background: #17171b;
  }

  .tab.active {
    background: var(--bg-panel);
    border-color: transparent;
  }

  /* Active tab is marked by the OMEN gradient along its top edge. */
  .tab.active::before {
    content: "";
    position: absolute;
    inset: 0 0 auto 0;
    height: 3px;
    background: var(--gradient);
    border-radius: 4px 4px 0 0;
  }

  .arrow {
    display: grid;
    place-items: center;
    width: 34px;
    border: none;
    background: transparent;
    color: var(--text-dim);
  }

  .arrow:hover {
    color: var(--text);
  }

  .arrow.hidden {
    visibility: hidden;
  }
</style>
