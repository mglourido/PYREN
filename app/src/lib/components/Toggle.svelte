<script lang="ts">
  /** Pill switch. Optional labels sit either side, as in the fan-speed row. */
  type Props = {
    checked: boolean;
    onchange: (value: boolean) => void;
    labelOff?: string;
    labelOn?: string;
    disabled?: boolean;
    ariaLabel?: string;
  };
  let { checked, onchange, labelOff, labelOn, disabled = false, ariaLabel }: Props = $props();
</script>

<div class="row" class:disabled>
  {#if labelOff}
    <span class="side" class:strong={!checked}>{labelOff}</span>
  {/if}
  <button
    class="switch"
    class:on={checked}
    {disabled}
    role="switch"
    aria-checked={checked}
    aria-label={ariaLabel}
    onclick={() => onchange(!checked)}
  >
    <span class="knob"></span>
  </button>
  {#if labelOn}
    <span class="side" class:strong={checked}>{labelOn}</span>
  {/if}
</div>

<style>
  .row {
    display: inline-flex;
    align-items: center;
    gap: 10px;
  }

  .row.disabled {
    opacity: 0.5;
  }

  .side {
    font-size: 12px;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--text-mute);
  }

  .side.strong {
    color: var(--text);
    font-weight: 700;
  }

  .switch {
    width: 56px;
    height: 26px;
    padding: 3px;
    border: none;
    border-radius: var(--radius-pill);
    background: #4a4a52;
    display: flex;
    justify-content: flex-start;
    transition: background 0.15s ease;
  }

  .switch.on {
    background: var(--info);
    justify-content: flex-end;
  }

  .knob {
    width: 20px;
    height: 20px;
    border-radius: 50%;
    background: #fff;
    transition: transform 0.15s ease;
  }
</style>
