<script lang="ts">
  /**
   * Range input with the OMEN gradient filling the travelled part of the
   * track. Min/max captions sit either side like the power-limit sliders.
   */
  type Props = {
    value: number;
    min: number;
    max: number;
    step?: number;
    disabled?: boolean;
    gradient?: boolean;
    minLabel?: string;
    maxLabel?: string;
    ariaLabel?: string;
    onchange: (value: number) => void;
  };
  let {
    value,
    min,
    max,
    step = 1,
    disabled = false,
    gradient = true,
    minLabel,
    maxLabel,
    ariaLabel,
    onchange,
  }: Props = $props();

  const percent = $derived(max === min ? 0 : ((value - min) / (max - min)) * 100);
</script>

<div class="slider" class:disabled>
  {#if minLabel}<span class="cap">{minLabel}</span>{/if}
  <input
    type="range"
    {min}
    {max}
    {step}
    {value}
    {disabled}
    aria-label={ariaLabel}
    style="--pct:{percent}%"
    class:gradient
    oninput={(e) => onchange(Number(e.currentTarget.value))}
  />
  {#if maxLabel}<span class="cap">{maxLabel}</span>{/if}
</div>

<style>
  .slider {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
  }

  .slider.disabled {
    opacity: 0.5;
  }

  .cap {
    font-size: 12px;
    font-weight: 700;
    color: var(--text);
    white-space: nowrap;
  }

  input {
    flex: 1;
    appearance: none;
    height: 6px;
    border-radius: var(--radius-pill);
    background: #4a4a52;
    margin: 0;
  }

  input.gradient {
    background: linear-gradient(
        90deg,
        var(--accent-1) 0%,
        var(--accent-2) 50%,
        var(--accent-3) 100%
      )
      no-repeat;
    background-size: var(--pct) 100%;
    background-color: #4a4a52;
  }

  input:not(.gradient) {
    background: linear-gradient(90deg, var(--text) 0 var(--pct), #4a4a52 var(--pct) 100%);
  }

  input::-webkit-slider-thumb {
    appearance: none;
    width: 16px;
    height: 16px;
    border-radius: 50%;
    background: #fff;
    border: none;
    cursor: pointer;
  }

  input:disabled::-webkit-slider-thumb {
    cursor: not-allowed;
  }
</style>
