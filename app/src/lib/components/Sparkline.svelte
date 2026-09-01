<script lang="ts">
  /**
   * Small multi-series history plot, drawn as an SVG polyline over a
   * gridded panel to match the advanced-view graphs.
   *
   * Each series is normalised over its own `max` so signals with different
   * units (MHz, °C, %) can share one plot the way the reference app does.
   */
  import type { Series } from "$lib/stores/telemetry.svelte";

  type Props = {
    series: Series[];
    max?: number;
    height?: number;
    columns?: number;
    rows?: number;
  };
  let { series, max = 100, height = 110, columns = 5, rows = 1 }: Props = $props();

  const WIDTH = 300;

  function toPoints(values: number[]): string {
    if (values.length < 2) return "";
    const step = WIDTH / (values.length - 1);
    return values
      .map((v, i) => {
        const y = height - Math.min(1, Math.max(0, v / max)) * (height - 6) - 3;
        return `${(i * step).toFixed(1)},${y.toFixed(1)}`;
      })
      .join(" ");
  }
</script>

<div class="wrap">
  <svg viewBox="0 0 {WIDTH} {height}" preserveAspectRatio="none" style="height:{height}px">
    {#each Array(columns - 1) as _, i (i)}
      <line
        x1={((i + 1) * WIDTH) / columns}
        x2={((i + 1) * WIDTH) / columns}
        y1="0"
        y2={height}
        class="grid"
      />
    {/each}
    {#each Array(rows - 1) as _, i (i)}
      <line
        y1={((i + 1) * height) / rows}
        y2={((i + 1) * height) / rows}
        x1="0"
        x2={WIDTH}
        class="grid"
      />
    {/each}
    {#each series as s (s.label)}
      <polyline points={toPoints(s.values)} fill="none" stroke={s.color} stroke-width="1.6" />
    {/each}
  </svg>
</div>

<style>
  .wrap {
    border: 1px solid var(--line);
    background: var(--bg-inset);
  }

  svg {
    display: block;
    width: 100%;
  }

  .grid {
    stroke: var(--line);
    stroke-width: 1;
    vector-effect: non-scaling-stroke;
  }
</style>
