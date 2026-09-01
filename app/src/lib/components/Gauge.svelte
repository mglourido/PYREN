<script lang="ts">
  /**
   * Circular utilisation ring used on the vitals dashboard. The arc is a
   * stroked SVG circle with a gradient, and the unfilled remainder stays
   * visible as a dim track.
   */
  type Props = {
    value: number | null;
    label: string;
    size?: number;
    id?: string;
  };
  let { value, label, size = 190, id = Math.random().toString(36).slice(2) }: Props = $props();

  const radius = $derived(size / 2 - 8);
  const circumference = $derived(2 * Math.PI * radius);
  const clamped = $derived(value === null ? 0 : Math.min(100, Math.max(0, value)));
  const dash = $derived((clamped / 100) * circumference);
</script>

<div class="gauge" style="width:{size}px;height:{size}px">
  <svg width={size} height={size} viewBox="0 0 {size} {size}">
    <defs>
      <linearGradient id="ring-{id}" x1="0" y1="1" x2="1" y2="0">
        <stop offset="0%" stop-color="#7b2ff7" />
        <stop offset="55%" stop-color="#e5178c" />
        <stop offset="100%" stop-color="#ff8a00" />
      </linearGradient>
    </defs>
    <circle
      cx={size / 2}
      cy={size / 2}
      r={radius}
      fill="none"
      stroke="#3a2a3f"
      stroke-width="2.5"
    />
    <circle
      cx={size / 2}
      cy={size / 2}
      r={radius}
      fill="none"
      stroke="url(#ring-{id})"
      stroke-width="3"
      stroke-linecap="round"
      stroke-dasharray="{dash} {circumference}"
      transform="rotate(-90 {size / 2} {size / 2})"
    />
  </svg>
  <div class="inner">
    <div class="value">{value === null ? "--" : `${Math.round(clamped)}%`}</div>
    <div class="label">{label}</div>
  </div>
</div>

<style>
  .gauge {
    position: relative;
    display: grid;
    place-items: center;
  }

  svg {
    position: absolute;
    inset: 0;
  }

  .inner {
    text-align: center;
    padding: 0 22px;
  }

  .value {
    font-size: 40px;
    font-weight: 700;
    line-height: 1.1;
  }

  .label {
    margin-top: 6px;
    font-size: 13px;
    color: var(--text-dim);
  }
</style>
