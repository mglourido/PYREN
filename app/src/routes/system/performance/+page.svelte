<script lang="ts">
  /**
   * Performance control - the page this project exists for. Mirrors the
   * reference layout: power mode tiles on top, then a temperature/power
   * settings switch, the fan control, and the temperature readouts.
   *
   * Which controls are available depends on the selected mode: manual
   * power limits stay grouped with Performance / Unlimited, as on Windows.
   * Manual fan control (the "manual" and "curve" fan modes and the
   * editable curve) is offered in *every* power mode where the driver can
   * be told a speed - the daemon keeps one curve and applies it whatever
   * the power mode is, so gating it on Unlimited only hid a control that
   * already worked.
   */
  import { onMount, tick } from "svelte";
  import Banner from "$lib/components/Banner.svelte";
  import FanCurve from "$lib/components/FanCurve.svelte";
  import Icon from "$lib/components/Icon.svelte";
  import InfoTip from "$lib/components/InfoTip.svelte";
  import ModeCard from "$lib/components/ModeCard.svelte";
  import Readout from "$lib/components/Readout.svelte";
  import Segmented from "$lib/components/Segmented.svelte";
  import Slider from "$lib/components/Slider.svelte";
  import Toggle from "$lib/components/Toggle.svelte";
  import { daemon, errorText, type HotkeyStatus } from "$lib/api/daemon";
  import { t, tm } from "$lib/i18n/index.svelte";
  import { formatTemp } from "$lib/stores/settings.svelte";
  import { telemetry, tempColor } from "$lib/stores/telemetry.svelte";
  import { LIMITS, hardware, type FanMode, type PowerMode } from "$lib/stores/hardware.svelte";
  import type { FanReferenceSensor } from "$lib/api/daemon";

  const modes: { id: PowerMode; icon: string }[] = [
    { id: "eco", icon: "leaf" },
    { id: "balanced", icon: "diamond" },
    { id: "performance", icon: "bars" },
    { id: "unlimited", icon: "boltbars" },
  ];

  let subTab = $state<"temp" | "power">("temp");

  /**
   * The bound shortcut, as the daemon has it - the same value Settings
   * shows and lets the user rebind. Asked once on open (and again whenever
   * the page is revisited), because only the daemon can hear a key and
   * "Fn + P" is just this machine's factory guess. `null` while there is no
   * daemon to ask; `label` is `null` when nothing is bound.
   */
  let hotkey = $state<HotkeyStatus | null>(null);
  onMount(() => {
    void daemon
      .hotkeyStatus()
      .then((status) => (hotkey = status))
      .catch(() => (hotkey = null));
  });

  // No daemon to ask means the shortcut is unknowable from here - better to
  // drop the hint than to keep asserting a factory guess that may be wrong.
  const hotkeyText = $derived(
    hotkey === null
      ? null
      : hotkey.label
        ? t("performance.hotkey", { shortcut: hotkey.label })
        : t("performance.hotkeyUnset"),
  );

  const mode = $derived(hardware.state.powerMode);
  /** Manual power limits are Unlimited-only; manual fan control is not. */
  const unlimited = $derived(mode === "unlimited");
  const powerTabAvailable = $derived(mode === "performance" || mode === "unlimited");

  // Falling back out of a sub-tab the current mode doesn't offer keeps the
  // page from showing an empty section after switching to Eco/Balanced.
  $effect(() => {
    if (!powerTabAvailable && subTab === "power") subTab = "temp";
  });

  /** Machines with no platform profile / ppd / EPP can't switch modes. */
  const powerControlAvailable = $derived(
    hardware.power === null || hardware.power.backend.available.length > 0,
  );

  /**
   * Whether this machine's driver can be told a *speed*, as opposed to just
   * "max" or "let the firmware decide". Board 8D2F is the case that makes
   * this necessary: it exposes `pwm1_enable` and no `pwm1`, so max and auto
   * work and a percentage does not. Assumed available until the daemon says
   * otherwise, so the demo mode still shows the full UI.
   */
  const canSetSpeed = $derived(hardware.fan?.capabilities.setSpeed ?? true);

  /**
   * Why `canSetSpeed` is what it is. `ignored` is board 8D2F: `pwm1` is
   * there, the driver takes the write, and the embedded controller keeps
   * the fans on the firmware's own curve - so the two "no speed control"
   * cases need different sentences. One is "your driver is missing a
   * file", the other is "your driver has the file and your hardware
   * ignores it", and sending someone to install a driver they already have
   * is the wrong end of a long afternoon.
   */
  const speedControl = $derived(hardware.fan?.speedControl ?? "untested");

  /** The probe is loud and blocking, so it says so and reports what it found. */
  let probing = $state(false);
  let probeResult = $state<{ ok: boolean; text: string } | null>(null);

  async function probeSpeed() {
    probing = true;
    probeResult = null;
    try {
      const probe = await daemon.probeFanSpeedControl();
      hardware.observeFan(probe.status);
      probeResult =
        probe.verdict === "honoured"
          ? { ok: true, text: t("performance.probeHonoured", { rpm: probe.reachedRpm }) }
          : probe.verdict === "ignored"
            ? { ok: false, text: t("performance.probeIgnored") }
            : { ok: false, text: t("performance.probeInconclusive") };
    } catch (e) {
      probeResult = { ok: false, text: errorText(e) };
    } finally {
      probing = false;
    }
  }

  const fanModeOptions = $derived(
    canSetSpeed
      ? (["max", "auto", "manual", "curve"] as FanMode[])
      : (["auto", "max"] as FanMode[]),
  );

  const fanModeLabels: Record<FanMode, string> = {
    auto: "common.auto",
    max: "common.max",
    manual: "common.manual",
    curve: "common.curve",
  };

  /**
   * The real power envelope, from the daemon. `null` outside Tauri, where
   * the demo data has no business inventing watt figures for a machine
   * nobody has measured.
   */
  /** Never offer to cap the CPU below something the machine can run on. */
  const PL_FLOOR_W = 5;

  const powerLimits = $derived(hardware.power?.limits ?? null);

  /**
   * Which profile the sliders below are editing.
   *
   * The reference app only shows this sub-tab under Performance and
   * Unlimited, and that layout is kept - but the envelope is stored per
   * mode, so with no selector here an Eco profile could only ever be tuned
   * over IPC. `null` means "whichever mode the machine is in", so the tab
   * still opens on the current one and follows it if it changes.
   */
  let tuningModeChoice = $state<PowerMode | null>(null);
  const tuningMode = $derived(tuningModeChoice ?? mode);
  const currentTuning = $derived(powerLimits?.tuning?.[tuningMode] ?? null);

  /** Microwatts to whole watts, the unit everything above the daemon uses. */
  const toWatts = (uw: number | null | undefined) => (uw == null ? null : Math.round(uw / 1e6));

  /**
   * One row per tunable limit. PL4 is missing on purpose: the daemon leaves
   * the peak-power ceiling at stock, since it exists to keep the VRM in
   * spec and lowering it buys nothing a lower PL1 has not already bought.
   */
  const pl = $derived.by(() => {
    const stock = powerLimits?.stock;
    const tuning = currentTuning;
    if (!stock || !tuning) return [];
    const rows = [];
    const pl1Max = toWatts(stock.pl1Uw);
    const pl2Max = toWatts(stock.pl2Uw);
    if (pl1Max) {
      rows.push({
        key: "pl1" as const,
        label: "PL1",
        max: pl1Max,
        watts: Math.round((pl1Max * tuning.pl1Percent) / 100),
      });
    }
    if (pl2Max) {
      rows.push({
        key: "pl2" as const,
        label: "PL2",
        max: pl2Max,
        watts: Math.round((pl2Max * tuning.pl2Percent) / 100),
      });
    }
    return rows;
  });

  /**
   * The GPU is what gets hot first under a game, so a curve that can
   * follow it is worth having - but only where there is a sensor to
   * follow. A machine with integrated graphics only gets no selector
   * rather than a choice that silently does nothing.
   */
  const gpuSensorAvailable = $derived(hardware.fan?.gpuSensorAvailable ?? false);
  const referenceSensor = $derived(hardware.state.fanReferenceSensor);
  /** A card that is powered down reads nothing, and the daemon follows the
   *  CPU instead. Saying so is the difference between a fallback and a
   *  setting that looks ignored. */
  const fallenBackToCpu = $derived(
    referenceSensor === "gpu" && hardware.fan?.referenceSensorInUse === "cpu",
  );
  /** The marker on the curve has to be the temperature the curve is
   *  actually read at, or it points at the wrong part of the shape. */
  const curveTempC = $derived(
    referenceSensor === "gpu" && hardware.fan?.gpuTempC != null
      ? hardware.fan.gpuTempC
      : telemetry.cpuTempC,
  );

  /**
   * Which profile's curve the editor is showing.
   *
   * The same shape as `tuningModeChoice` above, and for the same reason:
   * each power profile has its own curve, so the editor has to say which
   * one is on screen. `null` means "whichever the machine is in", so it
   * opens on the running one and follows it if something else moves it.
   */
  let curveProfileChoice = $state<PowerMode | null>(null);
  const curveProfile = $derived(curveProfileChoice ?? mode);
  const editingCurve = $derived(hardware.curveFor(curveProfile));

  /** The fixed-speed slider belongs to `manual` and nothing else. */
  const showManualSlider = $derived(canSetSpeed && hardware.state.fanMode === "manual");
  /** The curve editor belongs to `curve` and nothing else - in `manual`
   *  the fans run at one number the curve has no part in. */
  const showCurve = $derived(canSetSpeed && hardware.state.fanMode === "curve");

  /**
   * The daemon's supervisor stays out of the way for a while after a manual
   * mode change, and reports how long it has left. That figure is a
   * snapshot from the last power-state read, not a live counter - left
   * alone it would sit at ~599 in every mode until the next read. Age it
   * against the wall clock here so it actually counts down, and drop the
   * line once it - or the auto-switch feature itself - is gone.
   */
  let clockTick = $state(Date.now());
  $effect(() => {
    if (!hardware.power?.auto.enabled || !hardware.power?.autoOverrideSecondsLeft) return;
    const id = setInterval(() => (clockTick = Date.now()), 1000);
    return () => clearInterval(id);
  });

  const autoOverrideSecondsLeft = $derived.by(() => {
    if (!hardware.power?.auto.enabled) return 0;
    const snapshot = hardware.power.autoOverrideSecondsLeft;
    if (!snapshot) return 0;
    const elapsed = Math.max(0, Math.floor((clockTick - hardware.powerReadAt) / 1000));
    return Math.max(0, snapshot - elapsed);
  });

  // When a shown countdown runs out, re-read once so the supervisor's
  // resumed state (and anything it switched) replaces the stale snapshot.
  // Gated on the line having been visible, so a sync can't re-arm itself.
  let countingDown = false;
  $effect(() => {
    if (autoOverrideSecondsLeft > 0) {
      countingDown = true;
    } else if (countingDown) {
      countingDown = false;
      void hardware.syncFromDaemon();
    }
  });

  /**
   * Leaving `curve` mode removes a tall block, and the scroll container
   * does not always pull the viewport back up on its own - it sits parked
   * past the new bottom until the user nudges it. Clamp it by hand
   * whenever the fan sub-controls shrink.
   */
  let pageEl = $state<HTMLDivElement | null>(null);
  const fanControlsTall = $derived(showCurve || showManualSlider);
  $effect(() => {
    void fanControlsTall;
    void tick().then(() => {
      const scroller = pageEl?.closest(".tab-content");
      if (!scroller) return;
      const max = scroller.scrollHeight - scroller.clientHeight;
      if (scroller.scrollTop > max) scroller.scrollTop = Math.max(0, max);
    });
  });
</script>

<div class="page" bind:this={pageEl}>
  <Banner kind="warning" title="⚠">{t("performance.warning")}</Banner>

  <div class="inner">
    <!-- Power mode ------------------------------------------------- -->
    <header class="mode-head">
      <h1 class="section-title">{t("performance.powerMode")}</h1>
      <InfoTip>
        {t("performance.modeDesc.balanced")}
      </InfoTip>
      {#if hotkeyText}
        <span class="hotkey">{hotkeyText}</span>
      {/if}

      <label class="auto-select">
        <select
          value={hardware.state.applyToOsPowerProfile ? "auto" : "manual"}
          onchange={(e) =>
            hardware.setApplyToOsProfile(e.currentTarget.value === "auto")}
        >
          <option value="auto">{t("performance.autoModeSettings")}</option>
          <option value="manual">{t("common.manual")}</option>
        </select>
      </label>
    </header>

    <div class="modes">
      {#each modes as item (item.id)}
        <ModeCard
          icon={item.icon}
          label={t(`performance.modes.${item.id}`)}
          selected={mode === item.id}
          disabled={!powerControlAvailable}
          onselect={() => hardware.setPowerMode(item.id)}
        />
      {/each}
    </div>

    <p class="mode-desc">{t(`performance.modeDesc.${mode}`)}</p>

    <!-- Say what actually happened, rather than assuming the write landed. -->
    {#if !powerControlAvailable}
      <p class="feedback warn">{t("performance.noPowerControl")}</p>
    {:else if hardware.lastError}
      <p class="feedback err">
        {t("performance.applyFailed", { error: hardware.lastError })}
      </p>
    {:else if hardware.lastApply?.applied.length}
      <p class="feedback ok">
        {t("performance.appliedVia", { mechanism: hardware.lastApply.applied.join(", ") })}
      </p>
    {/if}

    {#if hardware.power?.supply.hasBattery}
      <p class="feedback">
        {hardware.power.supply.onBattery ? t("performance.onBattery") : t("performance.onMains")}
        {#if hardware.power.supply.batteryPercent !== null}
          — {hardware.power.supply.batteryPercent}%
        {/if}
      </p>
    {/if}

    {#if autoOverrideSecondsLeft > 0}
      <p class="feedback">
        {t("performance.autoPaused", { seconds: autoOverrideSecondsLeft })}
      </p>
    {/if}

    {#if hardware.power?.lastAutoSwitch}
      <p class="feedback">
        {t("performance.lastAutoSwitch", { detail: tm(hardware.power.lastAutoSwitch) })}
      </p>
    {/if}

    <label class="apply-os">
      <input
        type="checkbox"
        checked={hardware.state.applyToOsPowerProfile}
        onchange={(e) => hardware.setApplyToOsProfile(e.currentTarget.checked)}
      />
      {t("performance.applyToOsPowerProfile")}
      <InfoTip>{t("performance.applyToOsProfileHint")}</InfoTip>
    </label>

    <hr class="rule" />

    <!-- Temperature / power sub-tabs -------------------------------- -->
    <Segmented
      variant="pill"
      value={subTab}
      options={[
        { value: "temp", label: t("performance.tempSettings") },
        { value: "power", label: t("performance.powerSettings"), disabled: !powerTabAvailable },
      ]}
      onchange={(v) => (subTab = v as "temp" | "power")}
    />

    {#if subTab === "temp"}
      <div class="fan-area">
        <h2 class="fan-title">{t("performance.fanSpeed")}</h2>

        {#if canSetSpeed}
          <Segmented
            value={hardware.state.fanMode}
            options={fanModeOptions.map((value) => ({
              value,
              label: t(fanModeLabels[value]),
            }))}
            onchange={(v) => hardware.setFanMode(v as FanMode)}
          />
        {:else}
          <Toggle
            checked={hardware.state.fanMode === "max"}
            onchange={(v) => hardware.setFanMode(v ? "max" : "auto")}
            labelOff={t("common.auto")}
            labelOn={t("common.max")}
            ariaLabel={t("performance.fanSpeed")}
          />
        {/if}

        <div class="rpm">
          <span class="digital">{telemetry.fanRpm} RPM</span>
          {#if telemetry.fanReverse}
            <span class="reverse"><Icon name="refresh" size={14} /> {t("performance.fanReverse")}</span>
          {/if}
        </div>

        {#if showManualSlider}
          <div class="manual">
            <div class="manual-slider">
              <Slider
                value={hardware.state.fanPercent}
                min={0}
                max={100}
                minLabel="0%"
                maxLabel="100%"
                ariaLabel={t("performance.fanSpeed")}
                onchange={(v) => hardware.setFanPercent(v)}
              />
              <span class="pct">{hardware.state.fanPercent}%</span>
            </div>
          </div>
        {/if}

        {#if showCurve}
          <div class="manual curve-editor">
            <h3 class="curve-title">{t("performance.fanCurve")}</h3>
            <p class="curve-desc">{t("performance.fanCurveDesc")}</p>

            <!-- Each profile has its own curve, so the editor has to say
                 which one is on screen - and whether editing it does
                 anything right now. Mirrors the power tab's selector. -->
            <div class="tuning-scope">
              <span class="limit-scope">{t("performance.curveBelongsTo")}</span>
              <Segmented
                value={curveProfile}
                options={modes.map(({ id }) => ({
                  value: id,
                  label: t(`performance.modes.${id}`),
                }))}
                onchange={(v) => (curveProfileChoice = v as PowerMode)}
              />
            </div>
            <p class="limit-scope">
              {curveProfile === mode
                ? t("performance.curveAppliesNow")
                : t("performance.curveAppliesLater", {
                    mode: t(`performance.modes.${curveProfile}`),
                  })}
            </p>

            {#if gpuSensorAvailable}
              <div class="sensor-row">
                <span class="sensor-label">
                  {t("performance.curveSensor")}
                  <InfoTip>{t("performance.curveSensorHint")}</InfoTip>
                </span>
                <Segmented
                  value={referenceSensor}
                  options={[
                    { value: "cpu", label: t("performance.sensorCpu") },
                    { value: "gpu", label: t("performance.sensorGpu") },
                  ]}
                  onchange={(v) => hardware.setFanReferenceSensor(v as FanReferenceSensor)}
                />
              </div>
              {#if fallenBackToCpu}
                <p class="fan-note">{t("performance.curveSensorAsleep")}</p>
              {/if}
            {/if}

            <!-- The marker sits at the temperature right now on every
                 profile's curve, not only the running one: "at 46 C this
                 shape would command 31%" is exactly what you want to see
                 while tuning a profile you are not currently in. The line
                 above already says whether it is the one in force. -->
            <FanCurve
              curve={editingCurve}
              currentTempC={curveTempC}
              onchange={(curve) => hardware.setFanCurve(curve, curveProfile)}
            />
          </div>
        {/if}

        <!-- Two different machines, two different sentences. See `speedControl`. -->
        {#if !canSetSpeed}
          <p class="fan-note">
            {speedControl === "ignored"
              ? t("performance.fanSpeedIgnored")
              : t("performance.fanSpeedUnavailable")}
          </p>
        {/if}

        <!-- The curve is only worth drawing if the fans follow it, and only
             a probe can say. Offered while that is unknown, and again after
             a refusal so a driver change can be re-tested. -->
        {#if hardware.fan && (speedControl !== "honoured")}
          <div class="probe">
            <button class="probe-btn" onclick={probeSpeed} disabled={probing}>
              {probing ? t("performance.probing") : t("performance.probeSpeed")}
            </button>
            <span class="fan-note">{t("performance.probeSpeedHint")}</span>
          </div>
          {#if probeResult}
            <p class="feedback {probeResult.ok ? 'ok' : 'err'}">{probeResult.text}</p>
          {/if}
        {/if}
      </div>
    {:else}
      <div class="power-area">
        {#if powerLimits?.available}
          <div class="tuning-scope">
            <span class="limit-scope">{t("performance.limitsBelongTo")}</span>
            <Segmented
              value={tuningMode}
              options={modes.map(({ id }) => ({
                value: id,
                label: t(`performance.modes.${id}`),
              }))}
              onchange={(v) => (tuningModeChoice = v as PowerMode)}
            />
          </div>
          <p class="limit-scope">
            {tuningMode === mode
              ? t("performance.limitsApplyNow")
              : t("performance.limitsApplyLater", {
                  mode: t(`performance.modes.${tuningMode}`),
                })}
          </p>
          <div class="limits">
            {#each pl as { key, label, watts, max } (key)}
              <div class="limit">
                <span class="limit-label">
                  {label}
                  <InfoTip>
                    {key === "pl1" ? t("performance.pl1Hint") : t("performance.pl2Hint")}
                  </InfoTip>
                </span>
                <Slider
                  value={watts}
                  min={PL_FLOOR_W}
                  max={max}
                  minLabel="{PL_FLOOR_W}W"
                  maxLabel="{max}W"
                  ariaLabel={label}
                  onchange={(v) =>
                    hardware.setPowerTuning(
                      key === "pl1" ? { mode: tuningMode, pl1W: v } : { mode: tuningMode, pl2W: v },
                    )}
                />
                <span class="limit-value">{watts}W</span>
              </div>
            {/each}

            {#if powerLimits.turboAvailable}
              <div class="limit">
                <span class="limit-label">
                  {t("performance.turbo")}
                  <InfoTip>{t("performance.turboHint")}</InfoTip>
                </span>
                <Toggle
                  checked={currentTuning?.turbo ?? true}
                  onchange={(v) => hardware.setPowerTuning({ mode: tuningMode, turbo: v })}
                  labelOn={t("performance.activated")}
                  ariaLabel={t("performance.turbo")}
                />
              </div>
            {/if}
          </div>
        {/if}

        <div class="limits">
          <div class="limit">
            <span class="limit-label">
              {t("performance.smartBoost")}
              <InfoTip>{t("performance.smartBoostHint")}</InfoTip>
            </span>
            <Toggle
              checked={hardware.state.smartBoostEnabled}
              onchange={(v) => hardware.set("smartBoostEnabled", v)}
              labelOn={t("performance.activated")}
              ariaLabel={t("performance.smartBoost")}
            />
            <Slider
              value={hardware.state.smartBoostW}
              min={LIMITS.smartBoostW.min}
              max={LIMITS.smartBoostW.max}
              disabled={!hardware.state.smartBoostEnabled}
              minLabel="{LIMITS.smartBoostW.min}W"
              maxLabel="{LIMITS.smartBoostW.max}W"
              ariaLabel={t("performance.smartBoost")}
              onchange={(v) => hardware.set("smartBoostW", v)}
            />
          </div>

          <div class="limit">
            <span class="limit-label">{t("performance.maxBatteryDrain")}</span>
            <Slider
              value={hardware.state.maxBatteryDrain}
              min={LIMITS.maxBatteryDrain.min}
              max={LIMITS.maxBatteryDrain.max}
              minLabel="{LIMITS.maxBatteryDrain.min}%"
              maxLabel="{LIMITS.maxBatteryDrain.max}%"
              ariaLabel={t("performance.maxBatteryDrain")}
              onchange={(v) => hardware.set("maxBatteryDrain", v)}
            />
          </div>

          {#if unlimited}
            <div class="limit">
              <span class="limit-label">{t("performance.chassisTempLimit")}</span>
              <Slider
                value={hardware.state.chassisTempLimit}
                min={LIMITS.chassisTempLimit.min}
                max={LIMITS.chassisTempLimit.max}
                minLabel="{LIMITS.chassisTempLimit.min}°C"
                maxLabel="{LIMITS.chassisTempLimit.max}°C"
                ariaLabel={t("performance.chassisTempLimit")}
                onchange={(v) => hardware.set("chassisTempLimit", v)}
              />
            </div>
          {/if}
        </div>
      </div>
    {/if}

    <hr class="rule" />

    <!-- Temperatures ------------------------------------------------ -->
    <div class="temps">
      <h2 class="section-title">{t("performance.systemTemperature")}</h2>
      <div class="temp-row">
        <Readout
          value={formatTemp(telemetry.cpuTempC)}
          caption={t("vitals.cpuTemp")}
          color={tempColor(telemetry.cpuTempC)}
        />
        <Readout
          value={formatTemp(telemetry.gpuTempC)}
          caption={t("vitals.gpuTemp")}
          color={tempColor(telemetry.gpuTempC)}
        />
        <Readout
          value={formatTemp(telemetry.chassisTempC)}
          caption={t("vitals.chassisTemp")}
          color={tempColor(telemetry.chassisTempC)}
        />
      </div>
    </div>
  </div>
</div>

<style>
  .page {
    display: flex;
    flex-direction: column;
  }

  .inner {
    padding: 24px 40px 48px;
  }

  .mode-head {
    display: flex;
    align-items: center;
    gap: 12px;
    margin-bottom: 22px;
  }

  .hotkey {
    color: var(--text-dim);
    font-size: 13px;
  }

  .auto-select {
    margin-left: auto;
  }

  select {
    appearance: none;
    min-width: 300px;
    padding: 7px 30px 7px 12px;
    background: #2a2a2e;
    color: var(--text);
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
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

  .modes {
    display: flex;
    justify-content: center;
    gap: 26px;
    flex-wrap: wrap;
  }

  .mode-desc {
    text-align: center;
    color: var(--text-mute);
    font-size: 13px;
    margin: 14px 0 0;
  }

  .feedback {
    text-align: center;
    margin: 6px 0 0;
    font-size: 12px;
    color: var(--text-mute);
  }

  .feedback.ok {
    color: var(--ok);
  }

  .feedback.warn {
    color: var(--warn);
  }

  .feedback.err {
    color: var(--danger);
  }

  .apply-os {
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 8px;
    margin-top: 18px;
    font-size: 13px;
    cursor: pointer;
  }

  .rule {
    border: none;
    border-top: 1px solid var(--line-soft);
    margin: 22px 0;
  }

  .fan-area {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 18px;
    padding: 34px 0 10px;
  }

  .fan-title {
    font-size: 17px;
    font-weight: 400;
  }

  .rpm {
    display: flex;
    align-items: center;
    gap: 12px;
    margin-top: 22px;
    font-size: 34px;
  }

  .reverse {
    display: flex;
    align-items: center;
    gap: 5px;
    font-family: var(--font);
    font-size: 13px;
    color: var(--warn);
  }

  .manual {
    width: min(720px, 100%);
    margin-top: 10px;
  }

  /* The editor and its controls are centred as blocks in the fan area;
     the label text inside them stays left-aligned. */
  .curve-editor {
    display: flex;
    flex-direction: column;
    align-items: center;
    text-align: left;
  }

  .curve-editor .tuning-scope,
  .curve-editor .sensor-row {
    justify-content: center;
  }

  .manual-slider {
    display: flex;
    align-items: center;
    gap: 14px;
  }

  .pct {
    min-width: 48px;
    font-weight: 700;
  }

  .curve-title {
    margin-top: 26px;
    font-size: 15px;
  }

  .curve-desc {
    margin: 4px 0 12px;
    color: var(--text-mute);
    font-size: 13px;
  }

  .limit-scope {
    margin: 0 0 12px;
    color: var(--text-mute);
    font-size: 13px;
  }

  /* The selector and its label sit on one line while there is room, and
     wrap rather than squeezing the four mode buttons. */
  .tuning-scope {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 10px;
    margin-bottom: 10px;
  }

  .tuning-scope .limit-scope {
    margin: 0;
  }

  .sensor-row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 10px;
    margin-bottom: 12px;
  }

  .sensor-label {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    color: var(--text-mute);
    font-size: 13px;
  }

  .fan-note {
    max-width: 62ch;
    margin: 14px 0 0;
    color: var(--text-mute);
    font-size: 13px;
    line-height: 1.5;
  }

  .probe {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: center;
    gap: 12px;
    max-width: 62ch;
  }

  .probe .fan-note {
    margin: 0;
    flex: 1 1 34ch;
  }

  .probe-btn {
    padding: 7px 14px;
    background: #2a2a2e;
    color: var(--text);
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    font: inherit;
    font-size: 13px;
    cursor: pointer;
    white-space: nowrap;
  }

  .probe-btn:hover:not(:disabled) {
    border-color: var(--text-dim);
  }

  .probe-btn:disabled {
    opacity: 0.6;
    cursor: default;
  }

  .power-area {
    display: flex;
    gap: 60px;
    flex-wrap: wrap;
    padding: 28px 0 10px;
  }

  .limits {
    flex: 1 1 340px;
    display: flex;
    flex-direction: column;
    gap: 24px;
    max-width: 460px;
  }

  .limit {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .limit-label {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 13px;
    font-weight: 600;
  }

  .limit-value {
    font-size: 12px;
    color: var(--text-dim);
  }

  .temps {
    display: flex;
    align-items: center;
    gap: 40px;
    flex-wrap: wrap;
  }

  .temp-row {
    flex: 1;
    display: flex;
    justify-content: space-around;
    gap: 30px;
  }
</style>
