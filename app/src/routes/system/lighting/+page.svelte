<script lang="ts">
  /**
   * Keyboard lighting. The zone model (four backlight zones plus
   * brightness) matches what the OMEN 4-zone keyboards expose; the daemon
   * side is planned as a second module ported from omen-rgb-linux, so this
   * page currently drives local state only.
   */
  import Segmented from "$lib/components/Segmented.svelte";
  import Slider from "$lib/components/Slider.svelte";
  import { t } from "$lib/i18n/index.svelte";
  import { hardware, type LightingMode } from "$lib/stores/hardware.svelte";

  let activeZone = $state(0);

  const modes: LightingMode[] = ["static", "breathing", "wave", "off"];
  const presets = [
    ["#e5178c", "#f2374b", "#ff8a00", "#ffd400"],
    ["#7b2ff7", "#2f8fff", "#2fd0ff", "#21e065"],
    ["#ffffff", "#ffffff", "#ffffff", "#ffffff"],
    ["#ff0000", "#00ff00", "#0000ff", "#ffffff"],
  ];

  const off = $derived(hardware.state.lightingMode === "off");

  function setZoneColor(color: string) {
    const zones = [...hardware.state.zoneColors];
    zones[activeZone] = color;
    hardware.set("zoneColors", zones);
  }

  function applyPreset(preset: string[]) {
    hardware.set("zoneColors", [...preset]);
  }

  /** 20 keys per zone, purely for the on-screen preview. */
  const zoneKeys = Array.from({ length: 4 }, (_, zone) =>
    Array.from({ length: 18 }, (_, i) => `${zone}-${i}`),
  );
</script>

<div class="lighting">
  <header class="head">
    <span class="label">{t("lighting.mode")}</span>
    <Segmented
      value={hardware.state.lightingMode}
      options={modes.map((m) => ({ value: m, label: t(`lighting.${m}`) }))}
      onchange={(v) => hardware.set("lightingMode", v as LightingMode)}
    />
  </header>

  <div class="stage">
    <!-- Keyboard preview: four zones, lit with the selected colours. -->
    <div class="keyboard" class:off>
      {#each zoneKeys as keys, zone (zone)}
        <button
          class="zone"
          class:active={activeZone === zone}
          aria-label={t("lighting.zone", { n: zone + 1 })}
          onclick={() => (activeZone = zone)}
          style="--glow:{hardware.state.zoneColors[zone]};
                 --alpha:{off ? 0 : hardware.state.brightness / 100}"
        >
          {#each keys as key (key)}
            <span class="key"></span>
          {/each}
        </button>
      {/each}
    </div>

    <p class="hint">{t("lighting.selectZone")}</p>

    <div class="controls" class:disabled={off}>
      <div class="control">
        <span class="control-label">{t("lighting.zone", { n: activeZone + 1 })}</span>
        <input
          type="color"
          value={hardware.state.zoneColors[activeZone]}
          disabled={off}
          aria-label={t("lighting.zone", { n: activeZone + 1 })}
          oninput={(e) => setZoneColor(e.currentTarget.value)}
        />
      </div>

      <div class="control wide">
        <span class="control-label">{t("lighting.brightness")}</span>
        <Slider
          value={hardware.state.brightness}
          min={0}
          max={100}
          disabled={off}
          minLabel="0%"
          maxLabel="100%"
          ariaLabel={t("lighting.brightness")}
          onchange={(v) => hardware.set("brightness", v)}
        />
      </div>

      <div class="control wide">
        <span class="control-label">{t("lighting.presets")}</span>
        <div class="presets">
          {#each presets as preset, i (i)}
            <button class="preset" disabled={off} onclick={() => applyPreset(preset)}>
              <!-- Keyed by position, not by colour: a preset may repeat a
                   colour (the white one is four identical swatches), and a
                   duplicate key aborts the render of this whole page. -->
              {#each preset as color, zone (zone)}
                <span style="background:{color}"></span>
              {/each}
            </button>
          {/each}
        </div>
      </div>
    </div>
  </div>
</div>

<style>
  /* `min-height` and not just `flex-direction`: the page is shorter than
     the tab area, and without it `.stage`'s black stops at the last
     control and the tab area's own grey fills the rest - the section
     looks like it only half changed. Same reason as on the graphics,
     network and keys pages, which paint a dark stage too. */
  .lighting {
    display: flex;
    flex-direction: column;
    min-height: 100%;
  }

  .head {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 14px 26px;
    background: #1f1f23;
    border-bottom: 1px solid var(--line-soft);
  }

  .label {
    color: var(--text-dim);
    font-size: 14px;
  }

  .stage {
    flex: 1;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 22px;
    padding: 46px 26px 40px;
    background: var(--omen-black);
  }

  .keyboard {
    display: flex;
    gap: 6px;
    padding: 20px;
    background: linear-gradient(180deg, #141417, #0b0b0d);
    border: 1px solid var(--line);
    border-radius: var(--radius);
  }

  .zone {
    display: grid;
    grid-template-columns: repeat(6, 1fr);
    gap: 4px;
    padding: 8px;
    border: 1px solid transparent;
    border-radius: 4px;
    background: transparent;
  }

  .zone.active {
    border-color: var(--text-mute);
  }

  .key {
    width: 22px;
    height: 20px;
    border-radius: 3px;
    background: color-mix(in srgb, var(--glow) calc(var(--alpha) * 100%), #1b1b1f);
    box-shadow: 0 0 10px color-mix(in srgb, var(--glow) calc(var(--alpha) * 55%), transparent);
  }

  .keyboard.off .key {
    box-shadow: none;
  }

  .hint {
    margin: 0;
    color: var(--text-dim);
    font-size: 14px;
  }

  .controls {
    display: grid;
    grid-template-columns: auto minmax(220px, 320px);
    gap: 18px 34px;
    align-items: center;
  }

  .controls.disabled {
    opacity: 0.5;
  }

  .control {
    display: flex;
    align-items: center;
    gap: 14px;
  }

  .control.wide {
    grid-column: span 2;
  }

  .control-label {
    min-width: 90px;
    font-size: 13px;
    color: var(--text-dim);
  }

  input[type="color"] {
    width: 54px;
    height: 30px;
    padding: 0;
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    background: transparent;
    cursor: pointer;
  }

  .presets {
    display: flex;
    gap: 10px;
  }

  .preset {
    display: flex;
    width: 76px;
    height: 26px;
    padding: 0;
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    overflow: hidden;
  }

  .preset span {
    flex: 1;
  }

  .preset:hover:not(:disabled) {
    border-color: var(--text);
  }
</style>
