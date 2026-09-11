<script lang="ts">
  /**
   * GPU mode switch. On Linux this maps onto the same three states the
   * firmware exposes (iGPU only / hybrid / dGPU), which take effect after
   * a session restart - so the page states that rather than pretending the
   * change is instant.
   */
  import Banner from "$lib/components/Banner.svelte";
  import ModeCard from "$lib/components/ModeCard.svelte";
  import { t } from "$lib/i18n/index.svelte";
  import { hardware, type GpuMode } from "$lib/stores/hardware.svelte";

  const modes: { id: GpuMode; icon: string }[] = [
    { id: "integrated", icon: "battery" },
    { id: "hybrid", icon: "leaf" },
    { id: "discrete", icon: "monitor" },
  ];

  const initial = hardware.state.gpuMode;
  const changed = $derived(hardware.state.gpuMode !== initial);
</script>

<div class="graphics">
  {#if changed}
    <Banner kind="info" title="i">{t("graphics.rebootNeeded")}</Banner>
  {/if}

  <div class="stage">
    <h1 class="title">{t("graphics.title")}</h1>

    <div class="modes">
      {#each modes as mode (mode.id)}
        <div class="option">
          <ModeCard
            icon={mode.icon}
            label={t(`graphics.${mode.id}`)}
            selected={hardware.state.gpuMode === mode.id}
            onselect={() => hardware.setGpuMode(mode.id)}
          />
          <p class="desc">{t(`graphics.${mode.id}Desc`)}</p>
        </div>
      {/each}
    </div>

    <!-- Say what actually happened, rather than assuming the write landed -
         the same rule the performance page follows for power modes. -->
    {#if hardware.lastError}
      <p class="feedback err">{t("graphics.applyFailed", { error: hardware.lastError })}</p>
    {/if}
  </div>

  <footer class="foot">
    <button class="reset" onclick={() => hardware.setGpuMode("hybrid")}>
      {t("common.reset")}
    </button>
  </footer>
</div>

<style>
  .graphics {
    flex: 1;
    display: flex;
    flex-direction: column;
    min-height: 100%;
  }

  .stage {
    flex: 1;
    padding: 40px 26px;
    background: var(--omen-black);
  }

  .title {
    text-align: center;
    font-size: 22px;
    font-weight: 400;
    margin-bottom: 40px;
  }

  .modes {
    display: flex;
    justify-content: center;
    gap: 44px;
    flex-wrap: wrap;
  }

  .option {
    display: flex;
    flex-direction: column;
    width: 260px;
  }

  /* Let the card fill the column so its edges line up with the blurb below
     it - otherwise the button shrinks to its content and the text overhangs. */
  .option :global(.mode) {
    width: 100%;
    min-width: 0;
  }

  .desc {
    /* 1px to match the card's transparent border, so the text lines up with
       the card's content box rather than its outer edge. */
    margin: 18px 0 0 1px;
    color: var(--text-dim);
    font-size: 14px;
    line-height: 1.45;
  }

  .feedback {
    text-align: center;
    margin: 28px 0 0;
    font-size: 12px;
  }

  .feedback.err {
    color: var(--danger);
  }

  .foot {
    padding: 14px 26px;
    background: var(--bg-card-hover);
  }

  .reset {
    padding: 10px 20px;
    border: none;
    border-radius: 2px;
    background: var(--invert-bg);
    color: var(--invert-text);
    font-size: 12px;
    font-weight: 700;
    letter-spacing: 0.05em;
    text-transform: uppercase;
  }
</style>
