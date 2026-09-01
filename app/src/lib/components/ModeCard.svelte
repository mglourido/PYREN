<script lang="ts">
  /** One power/GPU mode tile. Selected tiles get a gradient border. */
  import Icon from "./Icon.svelte";

  type Props = {
    icon: string;
    label: string;
    selected?: boolean;
    disabled?: boolean;
    onselect?: () => void;
  };
  let { icon, label, selected = false, disabled = false, onselect }: Props = $props();
</script>

<button class="mode" class:selected {disabled} onclick={() => onselect?.()}>
  <Icon name={icon} size={30} stroke={1.4} />
  <span class="label">{label}</span>
</button>

<style>
  .mode {
    position: relative;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 12px;
    min-width: 210px;
    height: 118px;
    padding: 12px;
    border: 1px solid transparent;
    border-radius: 2px;
    background: var(--bg-card);
    color: var(--text-mute);
  }

  .mode:hover:not(:disabled) {
    background: var(--bg-card-hover);
    color: var(--text-dim);
  }

  .mode:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }

  .label {
    font-size: 16px;
  }

  .mode.selected {
    background: #141416;
    color: var(--text);
  }

  /* Gradient outline: a padded pseudo-element rather than border-image, so
     the corner radius is preserved. */
  .mode.selected::after {
    content: "";
    position: absolute;
    inset: -2px;
    border-radius: 3px;
    padding: 2px;
    background: var(--gradient);
    -webkit-mask:
      linear-gradient(#000 0 0) content-box,
      linear-gradient(#000 0 0);
    mask:
      linear-gradient(#000 0 0) content-box,
      linear-gradient(#000 0 0);
    -webkit-mask-composite: xor;
    mask-composite: exclude;
    pointer-events: none;
  }
</style>
