<script lang="ts">
  import type { Snippet } from "svelte";
  import TabBar from "$lib/components/TabBar.svelte";
  let { children }: { children: Snippet } = $props();
</script>

<TabBar />
<div class="tab-content">
  {@render children()}
</div>

<style>
  .tab-content {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    background: var(--bg-panel);
  }
</style>
