<script lang="ts">
  /** Pill/box group of mutually exclusive options (mode selectors). */
  type Option = { value: string; label: string; disabled?: boolean };
  type Props = {
    options: Option[];
    value: string;
    variant?: "pill" | "box";
    onchange: (value: string) => void;
  };
  let { options, value, variant = "box", onchange }: Props = $props();
</script>

<div class="seg {variant}" role="tablist">
  {#each options as option (option.value)}
    <button
      role="tab"
      aria-selected={option.value === value}
      class:active={option.value === value}
      disabled={option.disabled}
      onclick={() => onchange(option.value)}
    >
      {option.label}
    </button>
  {/each}
</div>

<style>
  .seg {
    display: inline-flex;
    gap: 8px;
  }

  button {
    border: none;
    background: transparent;
    color: var(--text-mute);
    font-size: 12px;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    padding: 8px 16px;
  }

  button:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .box {
    background: var(--bg-inset);
    gap: 0;
  }

  .box button.active {
    background: var(--invert-bg);
    color: var(--invert-text);
    font-weight: 600;
  }

  .pill button {
    border-radius: var(--radius-pill);
    background: var(--bg-card-hover);
    color: var(--text-dim);
  }

  .pill button.active {
    background: var(--invert-bg);
    color: var(--invert-text);
    font-weight: 600;
  }
</style>
