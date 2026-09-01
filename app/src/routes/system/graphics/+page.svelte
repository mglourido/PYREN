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
  <div class="stage">
    <h1 class="title">{t("graphics.title")}</h1>

    <div class="modes">
      {#each modes as mode (mode.id)}
        <div class="option">
          <ModeCard
            icon={mode.icon}
            label={t(`graphics.${mode.id}`)}
            selected={hardware.state.gpuMode === mode.id}
            onselect={() => hardware.set("gpuMode", mode.id)}
          />
          <p class="desc">{t(`graphics.${mode.id}Desc`)}</p>
        </div>
      {/each}
    </div>

    {#if changed}
      <div class="notice">
        <Banner kind="info" title="i">{t("graphics.rebootNeeded")}</Banner>
      </div>
    {/if}
  </div>

  <footer class="foot">
    <button class="reset" onclick={() => hardware.set("gpuMode", "hybrid")}>
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
    width: 260px;
  }

  .desc {
    margin: 18px 4px 0;
    color: var(--text-dim);
    font-size: 14px;
    line-height: 1.45;
  }

  .notice {
    max-width: 900px;
    margin: 36px auto 0;
  }

  .foot {
    padding: 14px 26px;
    background: #303035;
  }

  .reset {
    padding: 10px 20px;
    border: none;
    border-radius: 2px;
    background: #f2f2f4;
    color: #17171a;
    font-size: 12px;
    font-weight: 700;
    letter-spacing: 0.05em;
    text-transform: uppercase;
  }
</style>
