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
   * The daemon has no effects engine - it writes colours and a brightness,
   * and that is all the firmware protocol carries - so there is no
   * breathing or wave here. Offering them would be a switch that does
   * nothing.
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
    type RgbProbe,
    type RgbStatus,
  } from "$lib/api/daemon";
  import { t, tm } from "$lib/i18n/index.svelte";
  import { telemetry } from "$lib/stores/telemetry.svelte";
  import { onMount } from "svelte";

  const ZONES = 4;

  /** A drag on the brightness slider is one ACPI write per pixel unless it
   *  is held back; the strip only has to catch up when the hand stops. */
  const BRIGHTNESS_DEBOUNCE_MS = 180;

  /** Local mode, not a daemon concept: the protocol carries colours and a
   *  brightness, so "off" is black at brightness 0 and "static" is the
   *  same colour four times. Naming them is for the user's benefit. */
  type Mode = "static" | "zones" | "off";

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

  const presets = [
    ["#e5178c", "#f2374b", "#ff8a00", "#ffd400"],
    ["#7b2ff7", "#2f8fff", "#2fd0ff", "#21e065"],
    ["#ffffff", "#ffffff", "#ffffff", "#ffffff"],
    ["#ff0000", "#00ff00", "#0000ff", "#ffffff"],
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
    if (next.brightness === 0 || isBlack(stored)) {
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
    return () => clearTimeout(brightnessTimer);
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
    void apply(next);
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

  function setBrightness(value: number) {
    brightness = value;
    clearTimeout(brightnessTimer);
    brightnessTimer = setTimeout(() => void apply(), BRIGHTNESS_DEBOUNCE_MS);
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
          <span class="detail">{lighting.unreachable}</span>
        {/if}
      </Banner>
    {/if}

    <!-- Preview: the bottom light strip, four zones, click one to aim the
         colour picker at it. -->
    <div class="bar" class:off>
      {#each Array.from({ length: ZONES }, (_, i) => i) as zone (zone)}
        <button
          class="zone"
          class:active={activeZone === zone && mode !== "static"}
          aria-label={t("lighting.zone", { n: zone + 1 })}
          onclick={() => (activeZone = zone)}
          style="--glow:{zones[zone] ?? '#000000'};
                 --alpha:{off ? 0 : brightness / 100}"
        ></button>
      {/each}
    </div>

    <p class="hint">
      {mode === "static" ? t("lighting.staticHint") : t("lighting.selectZone")}
    </p>

    <Panel>
      {#if !loaded}
        <p class="notice">{t("common.loading")}</p>
      {:else}
        <div class="controls" class:disabled={!available}>
          <div class="control">
            <span class="control-label">{t("lighting.mode")}</span>
            <Segmented
              value={mode}
              options={(["static", "zones", "off"] as Mode[]).map((m) => ({
                value: m,
                label: t(`lighting.${m}`),
                disabled: !available || busy,
              }))}
              onchange={(v) => setMode(v as Mode)}
            />
          </div>

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
            <span class="digital value">{brightness}%</span>
          </div>

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
                       a colour (the white one is four identical swatches),
                       and a duplicate key aborts this page's render. -->
                  {#each preset as color, zone (zone)}
                    <span style="background:{color}"></span>
                  {/each}
                </button>
              {/each}
            </div>
          </div>
        </div>
      {/if}
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
            <div>
              <strong>{dialect.id}</strong>
              <span class="transport">{dialect.transport}</span>
              <p>{dialect.detail}</p>
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
    max-width: 70ch;
    color: var(--text-dim);
    font-size: 14px;
    line-height: 1.5;
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
    gap: 12px;
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
    gap: 20px;
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
    padding: 6px 10px;
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    background: var(--bg-panel, #1f1f23);
    color: var(--text);
    font-size: 13px;
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

  .dialects strong {
    font-family: var(--font-mono, monospace);
  }

  .dialects .transport {
    margin-left: 8px;
    color: var(--text-mute);
    font-size: 12px;
  }

  .dialects p {
    margin: 2px 0 0;
    color: var(--text-mute);
    font-size: 12px;
  }

  .swatches {
    display: flex;
    gap: 6px;
  }

  .swatch {
    width: 22px;
    height: 22px;
    border: 1px solid var(--line);
    border-radius: 3px;
  }
</style>
