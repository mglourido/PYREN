<script lang="ts">
  /**
   * Editable temperature -> fan-speed curve. Points are dragged with the
   * mouse and moved with the arrow keys, and each point is clamped between
   * its neighbours so the curve can never become non-monotonic in
   * temperature (the daemon interpolates between points and would
   * otherwise get an ambiguous lookup - see `curveValueAt`).
   */
  import { curveValueAt, type CurvePoint } from "$lib/stores/hardware.svelte";

  type Props = {
    curve: CurvePoint[];
    currentTempC?: number | null;
    minTemp?: number;
    maxTemp?: number;
    onchange: (curve: CurvePoint[]) => void;
  };
  let { curve, currentTempC = null, minTemp = 30, maxTemp = 100, onchange }: Props = $props();

  const W = 620;
  const H = 260;
  // Right pad leaves room for the 100° label to sit under the last gridline
  // without spilling off the edge.
  const PAD = { top: 16, right: 24, bottom: 30, left: 40 };

  let svg = $state<SVGSVGElement | null>(null);
  let dragging = $state<number | null>(null);

  const plotW = W - PAD.left - PAD.right;
  const plotH = H - PAD.top - PAD.bottom;

  const x = (tempC: number) => PAD.left + ((tempC - minTemp) / (maxTemp - minTemp)) * plotW;
  const y = (percent: number) => PAD.top + (1 - percent / 100) * plotH;

  const sorted = $derived([...curve].sort((a, b) => a.tempC - b.tempC));
  const line = $derived(sorted.map((p) => `${x(p.tempC)},${y(p.percent)}`).join(" "));
  const area = $derived(
    sorted.length
      ? `${x(sorted[0].tempC)},${y(0)} ${line} ${x(sorted[sorted.length - 1].tempC)},${y(0)}`
      : "",
  );

  function movePoint(index: number, tempC: number, percent: number) {
    const next = sorted.map((p) => ({ ...p }));
    const lower = index > 0 ? next[index - 1].tempC + 1 : minTemp;
    const upper = index < next.length - 1 ? next[index + 1].tempC - 1 : maxTemp;
    next[index] = {
      tempC: Math.round(Math.min(upper, Math.max(lower, tempC))),
      percent: Math.round(Math.min(100, Math.max(0, percent))),
    };
    onchange(next);
  }

  function pointerMove(event: PointerEvent) {
    if (dragging === null || !svg) return;
    const rect = svg.getBoundingClientRect();
    const px = ((event.clientX - rect.left) / rect.width) * W;
    const py = ((event.clientY - rect.top) / rect.height) * H;
    movePoint(
      dragging,
      minTemp + ((px - PAD.left) / plotW) * (maxTemp - minTemp),
      (1 - (py - PAD.top) / plotH) * 100,
    );
  }

  function keyNudge(event: KeyboardEvent, index: number) {
    const point = sorted[index];
    const step = event.shiftKey ? 5 : 1;
    const moves: Record<string, [number, number]> = {
      ArrowLeft: [-step, 0],
      ArrowRight: [step, 0],
      ArrowUp: [0, step],
      ArrowDown: [0, -step],
    };
    const move = moves[event.key];
    if (!move) return;
    event.preventDefault();
    movePoint(index, point.tempC + move[0], point.percent + move[1]);
  }

  const targetPercent = $derived(
    currentTempC === null ? null : Math.round(curveValueAt(curve, currentTempC)),
  );
</script>

<svelte:window
  onpointermove={pointerMove}
  onpointerup={() => (dragging = null)}
/>

<svg bind:this={svg} viewBox="0 0 {W} {H}" class="curve" role="application" aria-label="fan curve">
  {#each [0, 25, 50, 75, 100] as percent (percent)}
    <line x1={PAD.left} x2={W - PAD.right} y1={y(percent)} y2={y(percent)} class="grid" />
    <text x={PAD.left - 8} y={y(percent) + 4} class="axis" text-anchor="end">{percent}</text>
  {/each}
  {#each [40, 50, 60, 70, 80, 90, 100] as temp (temp)}
    <line x1={x(temp)} x2={x(temp)} y1={PAD.top} y2={H - PAD.bottom} class="grid" />
    <text x={x(temp)} y={H - 10} class="axis" text-anchor="middle">{temp}°</text>
  {/each}

  <polygon points={area} class="area" />
  <polyline points={line} class="line" />

  {#if currentTempC !== null}
    <line x1={x(currentTempC)} x2={x(currentTempC)} y1={PAD.top} y2={H - PAD.bottom} class="now" />
    {#if targetPercent !== null}
      <circle cx={x(currentTempC)} cy={y(targetPercent)} r="5" class="now-dot" />
    {/if}
  {/if}

  {#each sorted as point, index (index)}
    <circle
      cx={x(point.tempC)}
      cy={y(point.percent)}
      r="8"
      class="handle"
      class:dragging={dragging === index}
      role="slider"
      tabindex="0"
      aria-label="{point.tempC}°C"
      aria-valuenow={point.percent}
      aria-valuemin="0"
      aria-valuemax="100"
      onpointerdown={() => (dragging = index)}
      onkeydown={(e) => keyNudge(e, index)}
    />
  {/each}
</svg>

<style>
  .curve {
    display: block;
    width: 100%;
    max-width: 680px;
    margin-inline: auto;
    touch-action: none;
  }

  .grid {
    stroke: var(--line-soft);
    stroke-width: 1;
  }

  .axis {
    fill: var(--text-mute);
    font-size: 11px;
  }

  .area {
    fill: rgba(229, 23, 140, 0.12);
  }

  .line {
    fill: none;
    stroke: url(#none);
    stroke: var(--accent-1);
    stroke-width: 2;
  }

  .handle {
    fill: #fff;
    stroke: var(--accent-2);
    stroke-width: 2;
    cursor: grab;
  }

  .handle.dragging,
  .handle:hover {
    fill: var(--accent-3);
    cursor: grabbing;
  }

  .now {
    stroke: var(--ok);
    stroke-width: 1;
    stroke-dasharray: 4 4;
  }

  .now-dot {
    fill: var(--ok);
  }
</style>
