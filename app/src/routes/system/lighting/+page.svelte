<script lang="ts">
  /**
   * Lighting: the 4-zone bottom light strip, driven through the daemon's
   * `rgb` module (`docs/01-ipc-protocol.md` §"`rgb` module").
   *
   * Two things this page has to keep straight, because getting either
   * wrong makes it lie about the machine:
   *
   * 1. **It is the lightbar, not the keyboard.** The source project drives
   *    two unrelated things - per-key RGB over USB HID and this light
   *    strip over ACPI-WMI - and which one a laptop has is not decided by
   *    its model name. Only the strip is driven here, so the preview is a
   *    strip; a per-key keyboard that turns up is reported and said to be
   *    undriven rather than quietly steering these controls.
   * 2. **"Unavailable" has several causes with different fixes**: no
   *    `hp-wmi` at all, no `acpi_call` module, or a firmware that was asked
   *    and refused *this dialect*. Only the last is close to a verdict on
   *    the hardware, and even then only for one of three protocols. The
   *    daemon tells them apart; this page shows which, rather than one
   *    grey "not supported".
   * 3. **There is no single OMEN lighting protocol.** Three unrelated ways
   *    of talking to these lights exist, the machine is asked in all three,
   *    and the first that answers is used. The panel at the bottom shows
   *    what each one said and lets the user pin one, because auto can only
   *    ever pick a dialect this build can *read* — and the person at the
   *    keyboard can see whether the lights actually changed.
   *
   * 4. **Effects are the daemon's, not the firmware's.** The protocol only
   *    carries colours, so an effect is the daemon rewriting the zones many
   *    times a second (`rgb.setEffect`). The preview here runs the same
   *    frames (`$lib/lighting-effects`) so what is on screen is what is on
   *    the keys - and it is four zones, not keys, which the page says.
   */
  import Banner from "$lib/components/Banner.svelte";
  import Icon from "$lib/components/Icon.svelte";
  import InfoTip from "$lib/components/InfoTip.svelte";
  import Panel from "$lib/components/Panel.svelte";
  import Segmented from "$lib/components/Segmented.svelte";
  import Slider from "$lib/components/Slider.svelte";
  import Toggle from "$lib/components/Toggle.svelte";
  import {
    daemon,
    errorText,
    type RgbDialectId,
    type RgbEffect,
    type RgbEffectKind,
    type RgbEffectList,
    type RgbProbe,
    type RgbStatus,
  } from "$lib/api/daemon";
  import { frame } from "$lib/lighting-effects";
  import { t, tm } from "$lib/i18n/index.svelte";
  import { telemetry } from "$lib/stores/telemetry.svelte";
  import { lightingPresets, type LightingPreset } from "$lib/stores/lighting-presets.svelte";
  import { onMount } from "svelte";

  const ZONES = 4;
  const MAX_SAVED = 15;

  /** A drag on the brightness slider is one ACPI write per pixel unless it
   *  is held back; the strip only has to catch up when the hand stops. */
  const BRIGHTNESS_DEBOUNCE_MS = 180;
  /** A speed drag or a colour being picked restarts the effect in the
   *  daemon; it only has to happen once the hand stops. */
  const EFFECT_DEBOUNCE_MS = 250;

  /** Local mode, not a daemon concept: the protocol carries colours and a
   *  brightness, so "off" is black at brightness 0 and "static" is the
   *  same colour four times. Naming them is for the user's benefit.
   *  "effect" is the one the daemon does hold (`status.effect`). */
  type Mode = "static" | "zones" | "effect" | "off";

  let status = $state<RgbStatus | null>(null);
  /** A **fresh** probe. `getStatus.capabilities` is the one the daemon took
   *  at startup, which is exactly the wrong answer on the machine where
   *  `acpi_call` has just been installed - so this page asks for its own
   *  and prefers it. */
  let probe = $state<RgbProbe | null>(null);
  let error = $state<string | null>(null);
  let busy = $state(false);
  let loaded = $state(false);

  /** What the firmware itself says the zones are, when asked. Null until
   *  the button is pressed - it costs four ACPI round trips, so it is not
   *  something to poll. */
  let readBack = $state<{ zones: string[]; dialect: RgbDialectId } | null>(null);
  let readError = $state<string | null>(null);

  let mode = $state<Mode>("static");
  let zones = $state<string[]>(["#e5178c", "#f2374b", "#ff8a00", "#7b2ff7"]);
  let brightness = $state(100);
  let activeZone = $state(0);

  let brightnessTimer: ReturnType<typeof setTimeout> | undefined;
  let effectTimer: ReturnType<typeof setTimeout> | undefined;

  /** What the daemon can run. Null until asked, and on a daemon too old to
   *  have effects - in which case the mode is not offered. */
  let effectList = $state<RgbEffectList | null>(null);
  let effect = $state<RgbEffect>({
    kind: "spectrum",
    colors: [],
    speed: 5,
    direction: "leftToRight",
  });
  let fps = $state(30);
  /** The bar's colours while an effect is shown, one animation frame at a
   *  time. */
  let preview = $state<string[]>(["#000000", "#000000", "#000000", "#000000"]);
  const effectInfo = $derived(effectList?.effects.find((e) => e.id === effect.kind) ?? null);
  const shown = $derived(mode === "effect" ? preview : zones);

  const presets = [
    ["#e5178c", "#f2374b", "#ff8a00", "#ffd400"],
    ["#7b2ff7", "#2f8fff", "#2fd0ff", "#21e065"],
    ["#00f5a0", "#00d9f5", "#7b2ff7", "#f72585"],
    ["#ff0000", "#00ff00", "#0000ff", "#ffffff"],
    ["#ff0000", "#ff6a00", "#ffae00", "#ffe600"],
    ["#003973", "#0074b7", "#00a8e8", "#90e0ef"],
    ["#ff00e5", "#00fff5", "#39ff14", "#faff00"],
  ];

  const capabilities = $derived(probe ?? status?.capabilities ?? null);
  const lighting = $derived(capabilities?.lighting ?? null);
  const dialects = $derived(lighting?.dialects ?? []);
  const perKey = $derived(capabilities?.perKey ?? null);
  /** True only when the firmware was asked and said yes. Everything on
   *  this page that writes is gated on it. */
  /** A pinned dialect counts as available even when it did not probe:
   *  that is what pinning means, and greying the controls out would make
   *  the setting decorative. */
  const available = $derived(
    !telemetry.demo && ((lighting?.present ?? false) || status?.dialect !== "auto"),
  );
  const off = $derived(mode === "off");

  function isBlack(colors: string[]): boolean {
    return colors.every((c) => c.toLowerCase() === "#000000");
  }

  /** The daemon's stored colours, read back into this page's controls.
   *  Skipped mid-write so a reply cannot yank a slider out of a hand. */
  function adopt(next: RgbStatus) {
    status = next;
    if (busy) return;
    const stored = next.zones.slice(0, ZONES);
    brightness = next.brightness;
    if (next.fps) fps = next.fps;
    if (next.effect) {
      effect = { ...next.effect, colors: [...next.effect.colors] };
      zones = stored;
      mode = "effect";
    } else if (next.brightness === 0 || isBlack(stored)) {
      // Keep the colours on screen so switching back on has something to
      // switch back to; only the mode says the lights are out.
      mode = "off";
    } else {
      zones = stored;
      mode = stored.every((c) => c === stored[0]) ? "static" : "zones";
    }
  }

  /** `askFirmware` re-probes as well as reading the stored state. That is
   *  an ACPI round trip, so it is done on arrival and on the button - not
   *  on a timer, on the file the fan cleaner writes through. */
  async function refresh(askFirmware = false) {
    try {
      if (askFirmware) probe = await daemon.rgbCapabilities();
      adopt(await daemon.rgbStatus());
      error = null;
    } catch (e) {
      error = errorText(e);
    } finally {
      loaded = true;
    }
  }

  onMount(() => {
    void refresh(true);
    daemon
      .rgbEffects()
      .then((list) => (effectList = list))
      .catch(() => (effectList = null));

    // The preview's clock. Only draws while an effect is on screen, and
    // stops with the page.
    const began = performance.now();
    let raf = requestAnimationFrame(function tick(now) {
      if (mode === "effect") preview = frame(effect, (now - began) / 1000);
      raf = requestAnimationFrame(tick);
    });
    return () => {
      cancelAnimationFrame(raf);
      clearTimeout(brightnessTimer);
      clearTimeout(effectTimer);
    };
  });

  /** Writes the current controls to the strip. `static` sends one colour
   *  rather than four identical ones, which is the call the daemon has for
   *  exactly this. */
  async function apply(next: Mode = mode) {
    if (!available) return;
    busy = true;
    error = null;
    try {
      if (next === "off") {
        status = await daemon.rgbOff();
      } else if (next === "static") {
        status = await daemon.setRgbStatic(zones[activeZone] ?? zones[0], brightness);
        zones = zones.map(() => zones[activeZone] ?? zones[0]);
      } else {
        status = await daemon.setRgbZones(zones, brightness);
      }
      mode = next;
      // A write that succeeded makes the last read stale, and a stale
      // read shown beside fresh controls is worse than none.
      readBack = null;
    } catch (e) {
      error = errorText(e);
    } finally {
      busy = false;
    }
  }

  function setMode(next: Mode) {
    // Leaving `off` re-sends the colours that were on screen, which is
    // what the user was looking at while the lights were out. Brightness
    // is zero after an `off`, and switching "on" to nothing is not on.
    if (next !== "off" && brightness === 0) brightness = 100;
    if (next === "effect") {
      void applyEffect();
      return;
    }
    void apply(next);
  }

  /** Starts `effect` in the daemon. It replaces whatever was running, so
   *  every change to the effect's settings comes through here. */
  async function applyEffect() {
    if (!available) return;
    clearTimeout(effectTimer);
    busy = true;
    error = null;
    try {
      status = await daemon.setRgbEffect($state.snapshot(effect), brightness, fps);
      mode = "effect";
      readBack = null;
    } catch (e) {
      error = errorText(e);
    } finally {
      busy = false;
    }
  }

  function scheduleEffect() {
    clearTimeout(effectTimer);
    effectTimer = setTimeout(() => void applyEffect(), EFFECT_DEBOUNCE_MS);
  }

  /** A new kind starts from its own default colours, not the last kind's:
   *  a wave's pulse-and-background pair means nothing to a breathing. */
  function setEffectKind(kind: RgbEffectKind) {
    const defaults = effectList?.effects.find((e) => e.id === kind)?.defaults;
    effect = { ...effect, kind, colors: [...(defaults?.colors ?? [])] };
    void applyEffect();
  }

  function setEffectColor(index: number, color: string) {
    const colors = [...effect.colors];
    colors[index] = color;
    effect = { ...effect, colors };
  }

  function addEffectColor() {
    const last = effect.colors[effect.colors.length - 1] ?? "#ffffff";
    effect = { ...effect, colors: [...effect.colors, last] };
    scheduleEffect();
  }

  function removeEffectColor(index: number) {
    effect = { ...effect, colors: effect.colors.filter((_, i) => i !== index) };
    scheduleEffect();
  }

  function setSpeed(value: number) {
    effect = { ...effect, speed: Math.round(value) };
    scheduleEffect();
  }

  function setDirection(direction: RgbEffect["direction"]) {
    effect = { ...effect, direction };
    void applyEffect();
  }

  function setFps(value: number) {
    fps = value;
    void applyEffect();
  }

  /** The sweep in or out. Each call answers once the sweep is over. */
  async function power(on: boolean) {
    busy = true;
    error = null;
    try {
      status = on ? await daemon.rgbPowerOn() : await daemon.rgbPowerOff();
    } catch (e) {
      error = errorText(e);
    } finally {
      busy = false;
    }
  }

  async function setPowerAnimation(enabled: boolean) {
    try {
      status = await daemon.setRgbPowerAnimation(enabled);
      error = null;
    } catch (e) {
      error = errorText(e);
    }
  }

  async function setBatteryFps(value: number) {
    try {
      status = await daemon.setRgbBatteryFps(value);
      error = null;
    } catch (e) {
      error = errorText(e);
    }
  }

  function setZoneColor(color: string) {
    if (mode === "static") {
      zones = zones.map(() => color);
    } else {
      const next = [...zones];
      next[activeZone] = color;
      zones = next;
    }
  }

  function applyPreset(preset: string[]) {
    zones = [...preset];
    void apply(preset.every((c) => c === preset[0]) ? "static" : "zones");
  }

  /** The user's own fifteen slots, as opposed to the seven fixed `presets`
   *  above: colours, brightness and (for an effect) its settings, exactly
   *  as they are on screen when "Save" is pressed. */
  const saved = $derived(lightingPresets.current.presets);

  /** Truncates from the middle rather than the end, so a long name still
   *  shows how it starts *and* how it ends - the two lines under a slot
   *  are 28 characters, together, worth keeping legible. */
  function middleEllipsis(name: string, max = 28): string {
    if (name.length <= max) return name;
    const keep = max - 1;
    const head = Math.ceil(keep / 2);
    const tail = Math.floor(keep / 2);
    return `${name.slice(0, head)}…${name.slice(name.length - tail)}`;
  }

  /** "{mode}-{effect if mode is effect}-{slot}", e.g. "Efecto-Espectro-1".
   *  Only the default - renaming replaces it outright. */
  function defaultPresetName(): string {
    const parts = [t(`lighting.${mode}`)];
    if (mode === "effect") parts.push(t(`lighting.effectNames.${effect.kind}`));
    parts.push(String(saved.length + 1));
    return parts.join("-");
  }

  function saveCurrentAsPreset() {
    if (mode === "off" || saved.length >= MAX_SAVED) return;
    const next: LightingPreset = {
      id: crypto.randomUUID(),
      name: defaultPresetName(),
      mode,
      zones: [...zones],
      brightness,
      effect: mode === "effect" ? $state.snapshot(effect) : null,
      fps: mode === "effect" ? fps : null,
    };
    lightingPresets.set([...saved, next]);
  }

  function removeSavedPreset(id: string) {
    lightingPresets.set(saved.filter((p) => p.id !== id));
  }

  function applySavedPreset(p: LightingPreset) {
    zones = [...p.zones];
    brightness = p.brightness;
    if (p.mode === "effect" && p.effect) {
      effect = { ...p.effect, colors: [...p.effect.colors] };
      if (p.fps) fps = p.fps;
      void applyEffect();
    } else {
      void apply(p.mode);
    }
  }

  /** The rename dialog: a centred box like `NotificationsPanel`, not a
   *  popover on the slot - there is no room for a text field on something
   *  76px wide. */
  let renameTarget = $state<LightingPreset | null>(null);
  let renameDraft = $state("");

  function openRename(p: LightingPreset) {
    renameTarget = p;
    renameDraft = p.name;
  }

  function closeRename() {
    renameTarget = null;
  }

  function confirmRename() {
    if (!renameTarget) return;
    const name = renameDraft.trim();
    if (name) {
      lightingPresets.set(saved.map((p) => (p.id === renameTarget!.id ? { ...p, name } : p)));
    }
    closeRename();
  }

  function onRenameKey(e: KeyboardEvent) {
    if (!renameTarget) return;
    if (e.key === "Escape") closeRename();
    if (e.key === "Enter") confirmRename();
  }

  /** Focuses the name field once, when the dialog mounts - a plain
   *  `autofocus` attribute trips the a11y linter, and this is the dialog's
   *  one input, so grabbing focus on open is expected, not a surprise. */
  function focusOnMount(node: HTMLInputElement) {
    node.focus();
    node.select();
  }

  /** Its own call rather than re-sending the colours: with an effect
   *  running, re-sending would stop it, and the slider is meant to dim it. */
  function setBrightness(value: number) {
    brightness = value;
    clearTimeout(brightnessTimer);
    brightnessTimer = setTimeout(async () => {
      if (!available) return;
      try {
        status = await daemon.setRgbBrightness(brightness);
        error = null;
      } catch (e) {
        error = errorText(e);
      }
    }, BRIGHTNESS_DEBOUNCE_MS);
  }

  async function readFromFirmware() {
    readError = null;
    busy = true;
    try {
      readBack = await daemon.rgbReadZones();
    } catch (e) {
      readBack = null;
      readError = errorText(e);
    } finally {
      busy = false;
    }
  }

  /** Re-probes both paths. The answer to "I have just installed
   *  acpi_call", which is why it exists as a button and not a poll. */
  async function reprobe() {
    busy = true;
    try {
      // Not through `refresh(true)`: `adopt` steps aside while `busy`, and
      // this is the one refresh whose whole point is to land.
      probe = await daemon.rgbCapabilities();
      error = null;
    } catch (e) {
      error = errorText(e);
    } finally {
      busy = false;
    }
    await refresh();
  }

  async function setDialect(dialect: "auto" | RgbDialectId) {
    busy = true;
    try {
      status = await daemon.setRgbDialect(dialect);
      // The daemon re-probes on this call; take the fresh answer rather
      // than showing the one from before the change.
      probe = await daemon.rgbCapabilities();
      error = null;
      readBack = null;
    } catch (e) {
      error = errorText(e);
    } finally {
      busy = false;
    }
  }

  async function setRestoreOnStart(enabled: boolean) {
    try {
      status = await daemon.setRgbRestoreOnStart(enabled);
      error = null;
    } catch (e) {
      error = errorText(e);
    }
  }

  /** Which unavailable state this machine is in. `null` when something
   *  answered, or when the user has pinned a dialect and taken the
   *  question out of this page's hands. */
  const unavailable = $derived.by(() => {
    if (!lighting || lighting.present || status?.dialect !== "auto") return null;
    if (lighting.unreachable) return "unreachable" as const;
    if (!lighting.hpWmi && !dialects.some((d) => d.asked)) return "noWmi" as const;
    if (!lighting.acpiCall) {
      return lighting.acpiCallInstalled ? ("notLoaded" as const) : ("notInstalled" as const);
    }
    // Asked in every dialect and refused by each. The one distinction
    // worth keeping here: a firmware that answers the lighting command at
    // all is a machine with lights this build cannot yet speak to, which
    // is a different sentence from "your laptop has none".
    return lighting.commandAnswers ? ("wrongDialect" as const) : ("refused" as const);
  });
</script>

<svelte:window onkeydown={onRenameKey} />

<div class="lighting">
  <div class="stage">
    <h1 class="title">{t("lighting.title")}</h1>
    <p class="lead">{t("lighting.lead")}</p>

    {#if telemetry.demo}
      <p class="notice">{t("notices.daemonDownBody")}</p>
    {/if}

    {#if error}
      <p class="notice err">{error}</p>
    {:else if status?.error}
      <p class="notice err">{tm(status.error)}</p>
    {/if}

    {#if status?.dark}
      <div class="notice-row">
        <p class="notice">{t("lighting.dark")}</p>
        <button class="ghost" disabled={!available || busy} onclick={() => void power(true)}>
          <Icon name="bulb" size={15} />
          {t("lighting.powerOn")}
        </button>
      </div>
    {:else if status?.effect && !status.effectRunning && status.error}
      <p class="notice warn">{t("lighting.effectStopped")}</p>
    {:else if mode === "effect" && status?.throttled === "lid"}
      <p class="notice">{t("lighting.throttledLid")}</p>
    {:else if mode === "effect" && status?.throttled === "battery"}
      <p class="notice">
        {status.batteryFps === 0
          ? t("lighting.throttledBatteryPaused")
          : t("lighting.throttledBattery", { fps: status.batteryFps })}
      </p>
    {/if}

    {#if loaded && unavailable}
      <!-- The distinction this page exists to keep visible: a missing
           kernel module is not a verdict on the laptop. -->
      <Banner kind={unavailable === "refused" || unavailable === "noWmi" ? "info" : "warning"}>
        {t(`lighting.${unavailable}`)}
        <!-- The daemon's own words, only where they carry something the
             sentence above cannot: `unreachable` embeds *why* the call
             failed. The other four cases are fully said in the user's
             language and repeating them in English adds noise. -->
        {#if unavailable === "unreachable" && lighting?.unreachable}
          <span class="detail">{tm(lighting.unreachable)}</span>
        {/if}
      </Banner>
    {/if}

    <!-- The strip preview, the zone hint and the colour/effect controls,
         as one grouped block: they are all "editing" one thing, and three
         separate pieces of chrome on the bare stage read as unrelated. -->
    <Panel>
      <div class="edit-group">
        <div class="bar" class:off>
          {#each Array.from({ length: ZONES }, (_, i) => i) as zone (zone)}
            <button
              class="zone"
              class:active={activeZone === zone && mode !== "static"}
              aria-label={t("lighting.zone", { n: zone + 1 })}
              onclick={() => (activeZone = zone)}
              style="--glow:{shown[zone] ?? '#000000'};
                     --alpha:{off ? 0 : brightness / 100}"
            ></button>
          {/each}
        </div>

        <p class="hint">
          {mode === "static"
            ? t("lighting.staticHint")
            : mode === "effect"
              ? t("lighting.effectZonesNote")
              : t("lighting.selectZone")}
        </p>

        {#if !loaded}
          <p class="notice">{t("common.loading")}</p>
        {:else}
          <div class="controls" class:disabled={!available}>
          <div class="control">
            <span class="control-label">{t("lighting.mode")}</span>
            <Segmented
              value={mode}
              options={(
                (effectList ? ["static", "zones", "effect", "off"] : ["static", "zones", "off"]) as Mode[]
              ).map((m) => ({
                value: m,
                label: t(`lighting.${m}`),
                disabled: !available || busy,
              }))}
              onchange={(v) => setMode(v as Mode)}
            />
          </div>

          {#if mode === "effect" && effectList}
            <div class="control">
              <span class="control-label">{t("lighting.effectKind")}</span>
              <Segmented
                value={effect.kind}
                options={effectList.effects.map((e) => ({
                  value: e.id,
                  label: t(`lighting.effectNames.${e.id}`),
                  disabled: !available || busy,
                }))}
                onchange={(v) => setEffectKind(v as RgbEffectKind)}
              />
            </div>
            <p class="hint effect-hint">{t(`lighting.effectHints.${effect.kind}`)}</p>

            <div class="control">
              <span class="control-label">{t("lighting.colours")}</span>
              {#if effectInfo?.usesColors}
                <div class="effect-colours">
                  {#each effect.colors as color, i (i)}
                    <span class="effect-colour">
                      <input
                        type="color"
                        value={color}
                        disabled={!available}
                        aria-label={t("lighting.colour")}
                        oninput={(e) => setEffectColor(i, e.currentTarget.value)}
                        onchange={() => void applyEffect()}
                      />
                      {#if effect.colors.length > 1}
                        <button
                          class="remove"
                          disabled={!available || busy}
                          aria-label={t("lighting.removeColour")}
                          title={t("lighting.removeColour")}
                          onclick={() => removeEffectColor(i)}
                        >
                          <Icon name="close" size={11} />
                        </button>
                      {/if}
                    </span>
                  {/each}
                  {#if effect.colors.length < effectList.maxColors}
                    <button
                      class="ghost add"
                      disabled={!available || busy}
                      aria-label={t("lighting.addColour")}
                      title={t("lighting.addColour")}
                      onclick={addEffectColor}>+</button
                    >
                  {/if}
                </div>
              {:else}
                <span class="hint">{t("lighting.noColours")}</span>
              {/if}
            </div>

            <div class="control">
              <span class="control-label">{t("lighting.speed")}</span>
              <Slider
                value={effect.speed}
                min={effectList.speed.min}
                max={effectList.speed.max}
                step={1}
                disabled={!available}
                minLabel={String(effectList.speed.min)}
                maxLabel={String(effectList.speed.max)}
                ariaLabel={t("lighting.speed")}
                onchange={setSpeed}
              />
              <span class="digital value">{effect.speed}</span>
            </div>

            {#if effect.kind === "rainbowWave" || effect.kind === "wave"}
              <div class="control">
                <span class="control-label">{t("lighting.direction")}</span>
                <Segmented
                  value={effect.direction}
                  options={(["leftToRight", "rightToLeft"] as const).map((d) => ({
                    value: d,
                    label: t(`lighting.${d}`),
                    disabled: !available || busy,
                  }))}
                  onchange={(v) => setDirection(v as RgbEffect["direction"])}
                />
              </div>
            {/if}

            <div class="control">
              <span class="control-label">
                {t("lighting.fps")}
                <InfoTip>{t("lighting.fpsHint")}</InfoTip>
              </span>
              <Segmented
                value={String(fps)}
                options={[15, 30, 60].map((f) => ({
                  value: String(f),
                  label: `${f} fps`,
                  disabled: !available || busy,
                }))}
                onchange={(v) => setFps(Number(v))}
              />
            </div>
          {:else}
          <div class="control">
            <span class="control-label">
              {mode === "static" ? t("lighting.allZones") : t("lighting.zone", { n: activeZone + 1 })}
            </span>
            <input
              type="color"
              value={mode === "static" ? (zones[0] ?? "#000000") : (zones[activeZone] ?? "#000000")}
              disabled={!available || off}
              aria-label={t("lighting.colour")}
              oninput={(e) => setZoneColor(e.currentTarget.value)}
              onchange={() => void apply()}
            />
            <span class="hint inline">{t("lighting.colourHint")}</span>
          </div>
          {/if}

          <div class="control">
            <span class="control-label">{t("lighting.brightness")}</span>
            <Slider
              value={brightness}
              min={0}
              max={100}
              disabled={!available || off}
              minLabel="0%"
              maxLabel="100%"
              ariaLabel={t("lighting.brightness")}
              onchange={setBrightness}
            />
          </div>

          {#if mode === "zones"}
          <div class="control">
            <span class="control-label">{t("lighting.presets")}</span>
            <div class="presets">
              {#each presets as preset, i (i)}
                <button
                  class="preset"
                  disabled={!available || busy}
                  onclick={() => applyPreset(preset)}
                >
                  <!-- Keyed by position, not by colour: a preset may repeat
                       a colour, and a duplicate key aborts this page's
                       render. -->
                  {#each preset as color, zone (zone)}
                    <span style="background:{color}"></span>
                  {/each}
                </button>
              {/each}
            </div>
          </div>
          {/if}
        </div>
        {/if}
      </div>
    </Panel>

    <!-- Five slots for whatever is on screen above - separate from the
         fixed presets inside the controls panel, which never change. -->
    <Panel title={t("lighting.saved")}>
      {#snippet header()}
        <button
          class="ghost"
          disabled={!available || busy || mode === "off" || saved.length >= MAX_SAVED}
          onclick={saveCurrentAsPreset}
        >
          {t("lighting.saveButton", { used: saved.length, max: MAX_SAVED })}
        </button>
      {/snippet}
      <div class="saved-row">
        {#if saved.length === 0}
          <p class="hint">{t("lighting.savedEmpty")}</p>
        {:else}
          {#each saved as p (p.id)}
            <div class="saved-slot">
              <button
                class="preset saved-swatch"
                disabled={!available || busy}
                onclick={() => applySavedPreset(p)}
              >
                {#each Array.from({ length: ZONES }, (_, i) => p.zones[i] ?? "#000000") as color, zone (zone)}
                  <span style="background:{color}"></span>
                {/each}
              </button>
              <button
                class="remove saved-remove"
                disabled={busy}
                aria-label={t("lighting.removeSaved")}
                title={t("lighting.removeSaved")}
                onclick={() => removeSavedPreset(p.id)}
              >
                <Icon name="close" size={11} />
              </button>
              <button
                class="saved-name"
                aria-label={t("lighting.renameSaved")}
                title={p.name}
                onclick={() => openRename(p)}
              >
                <span>{middleEllipsis(p.name)}</span>
              </button>
            </div>
          {/each}
        {/if}
      </div>
    </Panel>

    <!-- The dialects. Not a debug panel: on a machine where auto picks
         nothing, this is the whole remaining path to working lights, and
         it is the only place the ids the daemon speaks are written down. -->
    <Panel title={t("lighting.protocol")}>
      <p class="lead small">{t("lighting.protocolLead")}</p>

      <div class="setting">
        <span class="label">
          {t("lighting.dialect")}
          <InfoTip>{t("lighting.dialectHint")}</InfoTip>
        </span>
        <div class="control-row">
          <select
            value={status?.dialect ?? "auto"}
            disabled={telemetry.demo || busy}
            aria-label={t("lighting.dialect")}
            onchange={(e) => void setDialect(e.currentTarget.value as "auto" | RgbDialectId)}
          >
            <option value="auto">
              {status?.activeDialect
                ? t("lighting.autoUsing", { dialect: status.activeDialect })
                : t("lighting.autoNothing")}
            </option>
            {#each dialects as dialect (dialect.id)}
              <option value={dialect.id}>
                {dialect.id}{dialect.available ? "" : ` — ${t("lighting.noAnswer")}`}
              </option>
            {/each}
          </select>
        </div>
      </div>

      <ul class="dialects">
        {#each dialects as dialect (dialect.id)}
          <li class:ok={dialect.available}>
            <span class="mark">
              {#if dialect.available}
                <Icon name="check" size={14} />
              {:else}
                <Icon name={dialect.asked ? "close" : "minimize"} size={14} />
              {/if}
            </span>
            <div class="info">
              <strong>{dialect.id}</strong>
              <span class="transport">{tm(dialect.transport)}</span>
              <p>{tm(dialect.detail)}</p>
            </div>
          </li>
        {/each}
      </ul>
    </Panel>

    <Panel title={t("lighting.hardware")}>
      <div class="setting">
        <span class="label">
          {t("lighting.restoreOnStart")}
          <InfoTip>{t("lighting.restoreOnStartHint")}</InfoTip>
        </span>
        <Toggle
          checked={status?.restoreOnStart ?? false}
          disabled={!available}
          onchange={(v) => void setRestoreOnStart(v)}
          ariaLabel={t("lighting.restoreOnStart")}
        />
      </div>

      <div class="setting">
        <span class="label">
          {t("lighting.powerAnimation")}
          <InfoTip>{t("lighting.powerAnimationHint")}</InfoTip>
        </span>
        <div class="control-row">
          <button class="ghost" disabled={!available || busy} onclick={() => void power(false)}>
            {t("lighting.powerOff")}
          </button>
          <button class="ghost" disabled={!available || busy} onclick={() => void power(true)}>
            {t("lighting.powerOn")}
          </button>
          <Toggle
            checked={status?.powerAnimation ?? false}
            disabled={!available}
            onchange={(v) => void setPowerAnimation(v)}
            ariaLabel={t("lighting.powerAnimation")}
          />
        </div>
      </div>

      {#if effectList}
        <div class="setting">
          <span class="label">
            {t("lighting.batteryFps")}
            <InfoTip>{t("lighting.batteryFpsHint")}</InfoTip>
          </span>
          <Segmented
            value={String(status?.batteryFps ?? 15)}
            options={[0, 15, 30].map((f) => ({
              value: String(f),
              label: f === 0 ? t("lighting.pause") : `${f} fps`,
              disabled: !available,
            }))}
            onchange={(v) => void setBatteryFps(Number(v))}
          />
        </div>
      {/if}

      <div class="setting">
        <span class="label">
          {t("lighting.readBack")}
          <InfoTip>{t("lighting.readBackHint")}</InfoTip>
        </span>
        <div class="control-row">
          <button class="ghost" disabled={!available || busy} onclick={readFromFirmware}>
            <Icon name="refresh" size={15} />
            {t("lighting.read")}
          </button>
          {#if readBack}
            <div class="swatches">
              {#each readBack.zones as color, zone (zone)}
                <span class="swatch" style="background:{color}" title={color}></span>
              {/each}
            </div>
            <span class="hint">{readBack.dialect}</span>
          {:else if readError}
            <span class="hint err">{readError}</span>
          {/if}
        </div>
      </div>

      <div class="setting">
        <span class="label">{t("lighting.probe")}</span>
        <div class="control-row">
          <button class="ghost" disabled={telemetry.demo || busy} onclick={reprobe}>
            <Icon name="search" size={15} />
            {t("lighting.reprobe")}
          </button>
          <span class="hint">
            {status?.owned ? t("lighting.owned") : t("lighting.notOwned")}
          </span>
        </div>
      </div>

      {#if readBack?.dialect === "fourZone"}
        <!-- Live on this project's own laptop, and worth a sentence rather
             than a mystery: the colour written to zone 4 is real, it just
             cannot be read back. -->
        <p class="notice warn">{t("lighting.zoneFourUnreadable")}</p>
      {/if}

      {#if perKey?.present}
        <!-- A keyboard that is here and is not driven. Saying nothing
             would read as this page controlling it. -->
        <p class="notice warn">{t("lighting.perKeyPresent")}</p>
      {/if}

      {#if status && !status.saved}
        <p class="notice err">{t("lighting.notSaved", { error: status.saveError ?? "" })}</p>
      {/if}
    </Panel>
  </div>
</div>

{#if renameTarget}
  <!-- Centred like `NotificationsPanel`: a box in the middle of the app,
       not a popover on a 76px-wide slot. -->
  <div class="backdrop">
    <button class="scrim" aria-label={t("common.close")} onclick={closeRename}></button>
    <div class="rename-panel" role="dialog" aria-modal="true" aria-label={t("lighting.renameSavedTitle")}>
      <header>
        <h2>{t("lighting.renameSavedTitle")}</h2>
        <button class="close" onclick={closeRename} aria-label={t("common.close")}>
          <Icon name="close" size={16} />
        </button>
      </header>
      <input
        type="text"
        bind:value={renameDraft}
        placeholder={t("lighting.savedNamePlaceholder")}
        aria-label={t("lighting.savedNamePlaceholder")}
        maxlength="24"
        use:focusOnMount
      />
      <div class="rename-actions">
        <button class="ghost" onclick={closeRename}>{t("common.cancel")}</button>
        <button class="ghost primary" disabled={!renameDraft.trim()} onclick={confirmRename}>
          {t("common.save")}
        </button>
      </div>
    </div>
  </div>
{/if}

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

  .stage {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 18px;
    padding: 26px;
    background: var(--omen-black);
  }

  .title {
    margin: 0;
    font-size: 22px;
  }

  .lead {
    margin: 0;
    color: var(--text-dim);
    font-size: 14px;
    line-height: 1.5;
  }

  /* The strip, its hint and the controls, stacked with the same gap the
     stage used to give them as loose siblings - grouping them into one
     panel shouldn't also change their spacing. */
  .edit-group {
    display: flex;
    flex-direction: column;
    gap: 18px;
  }

  /* The bottom light strip, drawn as one: four segments of a single bar,
     because that is the shape of the thing being controlled. */
  .bar {
    display: flex;
    gap: 3px;
    padding: 10px;
    background: linear-gradient(180deg, #141417, #0b0b0d);
    border: 1px solid var(--line);
    border-radius: var(--radius);
  }

  .zone {
    flex: 1;
    height: 34px;
    border: 1px solid transparent;
    border-radius: 4px;
    cursor: pointer;
    background: color-mix(in srgb, var(--glow) calc(var(--alpha) * 100%), #131316);
    box-shadow: 0 6px 22px color-mix(in srgb, var(--glow) calc(var(--alpha) * 55%), transparent);
  }

  .zone.active {
    border-color: var(--text);
  }

  .bar.off .zone {
    box-shadow: none;
  }

  .hint {
    margin: 0;
    color: var(--text-dim);
    font-size: 13px;
  }

  .hint.inline {
    margin-left: 4px;
  }

  .hint.err,
  .notice.err {
    color: var(--danger, #f2374b);
  }

  .notice {
    margin: 0;
    color: var(--text-dim);
    font-size: 13px;
  }

  .notice.warn {
    color: var(--warning, #ffb020);
  }

  .detail {
    display: block;
    margin-top: 4px;
    color: var(--text-mute);
    font-size: 12px;
  }

  .controls {
    display: flex;
    flex-direction: column;
    gap: 16px;
  }

  .controls.disabled {
    opacity: 0.5;
  }

  .control {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 8px 12px;
  }

  .control-label {
    min-width: 110px;
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

  input[type="color"]:hover {
    border-color: var(--text);
  }

  input[type="color"]::-webkit-color-swatch-wrapper {
    padding: 0;
  }

  input[type="color"]::-webkit-color-swatch {
    border: none;
    border-radius: calc(var(--radius-sm) - 1px);
  }

  input[type="color"]::-moz-color-swatch {
    border: none;
    border-radius: calc(var(--radius-sm) - 1px);
  }

  .value {
    min-width: 48px;
    text-align: right;
    font-size: 13px;
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

  .setting {
    display: flex;
    align-items: center;
    justify-content: space-between;
    flex-wrap: wrap;
    gap: 10px 20px;
    padding: 10px 0;
    border-bottom: 1px solid var(--line-soft);
  }

  .setting:last-child {
    border-bottom: none;
  }

  .label {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 13px;
    color: var(--text-dim);
  }

  .control-row {
    display: flex;
    align-items: center;
    gap: 12px;
  }

  .ghost {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 6px 12px;
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--text);
    font-size: 13px;
    cursor: pointer;
  }

  .ghost:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .lead.small {
    font-size: 13px;
    margin-bottom: 4px;
  }

  select {
    appearance: none;
    min-width: 220px;
    max-width: 100%;
    padding: 7px 30px 7px 12px;
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    background-color: var(--bg-card);
    color: var(--text);
    font: inherit;
    font-size: 13px;
    background-image: linear-gradient(45deg, transparent 50%, var(--text-dim) 50%),
      linear-gradient(135deg, var(--text-dim) 50%, transparent 50%);
    background-position:
      right 15px center,
      right 10px center;
    background-size:
      6px 6px,
      6px 6px;
    background-repeat: no-repeat;
  }

  /* The native popup list ignores the control's colours on some engines,
     so it needs its own dark background to match the theme. */
  select option {
    background: var(--bg-card);
    color: var(--text);
  }

  .dialects {
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin: 12px 0 0;
    padding: 0;
    list-style: none;
  }

  .dialects li {
    display: flex;
    gap: 10px;
    align-items: flex-start;
    color: var(--text-dim);
    font-size: 13px;
  }

  .dialects li.ok {
    color: var(--text);
  }

  .dialects .mark {
    flex: none;
    margin-top: 1px;
    opacity: 0.7;
  }

  .dialects .info {
    flex: 1;
    min-width: 0;
    width: 100%;
  }

  .dialects strong {
    color: var(--text);
    font-family: var(--font-mono, monospace);
  }

  .dialects .transport {
    margin-left: 8px;
    color: var(--text-dim);
    font-size: 12px;
    font-weight: 600;
  }

  .dialects p {
    width: 100%;
    margin: 2px 0 0;
    color: var(--text-mute);
    font-size: 12px;
  }

  .swatches {
    display: flex;
    gap: 6px;
  }

  .notice-row {
    display: flex;
    align-items: center;
    gap: 12px;
  }

  .effect-hint {
    margin-top: -8px;
    padding-left: 122px;
  }

  .effect-colours {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 10px;
  }

  .effect-colour {
    position: relative;
    display: inline-flex;
  }

  .effect-colour .remove,
  .saved-remove {
    position: absolute;
    top: -6px;
    right: -6px;
    display: grid;
    place-items: center;
    width: 16px;
    height: 16px;
    padding: 0;
    border: 1px solid var(--line);
    border-radius: 50%;
    background: var(--bg-card);
    color: var(--text-dim);
    cursor: pointer;
  }

  .effect-colour .remove:hover:not(:disabled),
  .saved-remove:hover:not(:disabled) {
    color: var(--text);
    border-color: var(--text);
  }

  .saved-row {
    display: flex;
    flex-wrap: wrap;
    gap: 16px;
    margin: 4px 0 14px;
  }

  .saved-slot {
    position: relative;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 6px;
    width: 108px;
  }

  .saved-swatch {
    width: 108px;
  }

  .saved-name {
    display: block;
    width: 100%;
    padding: 0;
    border: none;
    background: transparent;
    color: var(--text-dim);
    font-size: 11px;
    text-align: center;
    cursor: pointer;
  }

  .saved-name span {
    display: -webkit-box;
    overflow: hidden;
    -webkit-line-clamp: 2;
    -webkit-box-orient: vertical;
    line-clamp: 2;
    white-space: normal;
    word-break: break-word;
    line-height: 1.3;
  }

  .saved-name:hover {
    color: var(--text);
  }

  .backdrop {
    position: fixed;
    inset: 0;
    z-index: 200;
    display: grid;
    place-items: center;
    padding: 24px;
  }

  .scrim {
    position: absolute;
    inset: 0;
    border: none;
    padding: 0;
    background: rgba(0, 0, 0, 0.72);
    backdrop-filter: blur(3px);
    cursor: default;
  }

  .rename-panel {
    position: relative;
    width: min(360px, 100%);
    display: flex;
    flex-direction: column;
    gap: 14px;
    padding: 20px 22px;
    border: 1px solid var(--line);
    border-radius: var(--radius-lg);
    background: var(--bg-panel);
    box-shadow: var(--shadow);
  }

  .rename-panel header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
  }

  .rename-panel h2 {
    margin: 0;
    font-size: 1.05rem;
    font-weight: 600;
  }

  .rename-panel .close {
    display: grid;
    place-items: center;
    width: 26px;
    height: 26px;
    border: none;
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--text-dim);
    cursor: pointer;
  }

  .rename-panel .close:hover {
    background: var(--bg-card);
    color: var(--text);
  }

  .rename-panel input[type="text"] {
    padding: 8px 12px;
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    background: var(--bg-card);
    color: var(--text);
    font: inherit;
    font-size: 13px;
  }

  .rename-actions {
    display: flex;
    justify-content: flex-end;
    gap: 10px;
  }

  .ghost.primary {
    border-color: var(--accent-2, var(--text));
  }

  .ghost.add {
    width: 30px;
    height: 30px;
    justify-content: center;
    padding: 0;
    font-size: 16px;
  }

  .swatch {
    width: 22px;
    height: 22px;
    border: 1px solid var(--line);
    border-radius: 3px;
  }
</style>
