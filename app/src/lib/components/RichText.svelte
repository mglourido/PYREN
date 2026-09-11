<script lang="ts">
  /**
   * Renders a translated string that may carry `**bold**` spans, so a hint
   * can highlight the technical words (systemd, root, …) without us hand-
   * building markup in every locale. Only `**…**` is understood; everything
   * else is plain text, escaped by Svelte as usual - no `{@html}`.
   */
  const { text }: { text: string } = $props();

  // Odd segments are the ones that sat between a pair of `**`.
  const parts = $derived(text.split("**"));
</script>

{#each parts as part, i (i)}
  {#if i % 2 === 1}<strong>{part}</strong>{:else}{part}{/if}
{/each}
