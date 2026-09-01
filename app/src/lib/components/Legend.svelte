<script lang="ts">
  import type { Series } from "$lib/stores/telemetry.svelte";
  let { series }: { series: Series[] } = $props();
</script>

<ul class="legend">
  {#each series as s (s.label)}
    <li><span class="swatch" style="background:{s.color}"></span>{s.label}</li>
  {/each}
</ul>

<style>
  .legend {
    list-style: none;
    margin: 10px 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  li {
    display: flex;
    align-items: center;
    gap: 10px;
    font-size: 13px;
    font-weight: 600;
  }

  .swatch {
    width: 13px;
    height: 13px;
    border-radius: 2px;
  }
</style>
