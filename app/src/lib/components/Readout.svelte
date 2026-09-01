<script lang="ts">
  /** Large phosphor-style number (RPM, temperatures) with its caption. */
  type Props = { value: string; caption?: string; color?: string; size?: number };
  let { value, caption, color = "var(--ok)", size = 44 }: Props = $props();
</script>

<div class="readout">
  <span class="value digital" style="color:{color};font-size:{size}px">{value}</span>
  {#if caption}<span class="caption">{caption}</span>{/if}
</div>

<style>
  .readout {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 8px;
  }

  .value {
    line-height: 1;
    font-weight: 500;
  }

  .caption {
    color: var(--text-dim);
    font-size: 13px;
  }
</style>
