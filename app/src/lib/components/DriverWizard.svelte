<script lang="ts">
  /**
   * The kernel-driver installer, as a reviewable plan rather than a button.
   *
   * Installing means unloading a kernel module, replacing a file under
   * `/lib/modules` and regenerating the initramfs. The daemon already
   * splits that into inspect → plan → apply so the steps can be read
   * before they run (`docs/01-ipc-protocol.md`); this is that split made
   * visible, and it is the whole reason the page does not simply offer an
   * "Install" button.
   *
   * Three rules the UI enforces on top of the daemon's own:
   *
   * - It stays **closed by default**, because on a modern kernel manual fan
   *   control is upstream and replacing the stock driver is a downgrade.
   *   `inspect` says so (`patchNeeded: false`) and the summary leads with it.
   * - **Apply is unreachable until a dry run of the same options has been
   *   read.** Changing any option throws the dry run away, so the plan on
   *   screen is always the plan that would run.
   * - A plan with blockers is not offered at all; the blockers are shown
   *   with the command that fixes each one instead.
   *
   * Installing is offered as a **choice of two modes**, not one path with
   * an escape hatch:
   *
   * - **Automatic** asks the daemon what this machine is
   *   (`installer.autodetect` - the board id from DMI, the tables from the
   *   driver's own source, the fan ceilings from the last calibration) and
   *   dry-runs the plan those answers produce. It removes the *typing*,
   *   not the reading: what was detected, the reasoning behind each answer,
   *   and the steps all appear before anything is authorised.
   * - **Manual** is the same plan from values typed in, for someone who
   *   knows their board and disagrees with what was detected.
   *
   * The mode is part of the request, so switching it throws away a dry run
   * like any other option would, and the manual fields are not sent while
   * they are off screen - a value the user cannot see must not decide what
   * runs.
   */
  import Icon from "$lib/components/Icon.svelte";
  import Panel from "$lib/components/Panel.svelte";
  import Toggle from "$lib/components/Toggle.svelte";
  import Segmented from "$lib/components/Segmented.svelte";
  import { onDestroy, tick } from "svelte";
  import InstallProgress from "$lib/components/InstallProgress.svelte";
  import type { LiveStep } from "$lib/components/InstallProgress.svelte";
  import {
    daemon,
    type Autodetected,
    type BoardParams,
    type BoardTable,
    type ExecutionReport,
    type InstallerAction,
    type InstallerInspection,
    type InstallerRequest,
    type InstallerProgress,
    type InstallPlan,
    type InstallStep,
    type StepStatus,
    onDaemonEvent,
  } from "$lib/api/daemon";
  import { hardware } from "$lib/stores/hardware.svelte";
  import { t, tm } from "$lib/i18n/index.svelte";

  let open = $state(false);

  let inspection = $state<InstallerInspection | null>(null);
  let inspecting = $state(false);
  let inspectError = $state<string | null>(null);

  let action = $state<InstallerAction>("installDriver");

  /**
   * The two ways to install, offered as a choice up front rather than as a
   * button plus a form underneath it. They are genuinely different jobs -
   * one asks the machine what it is, the other is for the person who
   * already knows and disagrees - and showing both at once made the
   * automatic path look like a shortcut past the fields rather than the
   * normal way in.
   */
  type InstallMode = "automatic" | "manual";
  let mode = $state<InstallMode>("automatic");

  /**
   * What the request carries. Derived, not stored: restoring the stock
   * driver has nothing to detect, so the mode only means anything for an
   * install.
   */
  const auto = $derived(action === "installDriver" && mode === "automatic");
  let detected = $state<Autodetected | null>(null);
  let detecting = $state(false);
  let autoError = $state<string | null>(null);
  let preferHooks = $state(false);
  let force = $state(false);
  let cpuMaxRpm = $state<string>("");
  let gpuMaxRpm = $state<string>("");
  let experimentalBoard = $state<string>("");
  let boardTableName = $state<BoardTable["table"]>("features");
  let boardParams = $state<BoardParams>("victusS");

  /**
   * Optional steps the user has unticked. "Optional" is the plan's own
   * word for a step whose failure it tolerates - regenerating the
   * initramfs, unloading a module that may not be loaded - and those are
   * exactly the ones somebody who knows their own machine may want left
   * alone. Required steps have no checkbox, and the daemon refuses to skip
   * one even if asked.
   */
  let skipSteps = $state<string[]>([]);

  let plan = $state<InstallPlan | null>(null);
  let planning = $state(false);
  let planError = $state<string | null>(null);

  let report = $state<ExecutionReport | null>(null);
  let running = $state(false);
  let runError = $state<string | null>(null);

  /**
   * The run as it happens, built from the daemon's `installer.progress`
   * events rather than from the report, which only arrives at the end.
   *
   * Two reasons this exists rather than a spinner. The wait is long enough
   * - `dkms-build` is most of a minute on its own - that "is it working or
   * is it stuck" is a real question, and until the module is back the rest
   * of the app is talking to a daemon whose driver is unloaded, so the
   * window has to stop accepting clicks anyway. Somewhere that is being
   * covered up may as well say what it is doing.
   */
  let liveSteps = $state<LiveStep[]>([]);
  let liveCurrent = $state<number | null>(null);
  let liveLog = $state<string[]>([]);
  let overlayOpen = $state(false);
  /** Whether the run on screen is the real one or the rehearsal. */
  let liveConfirm = $state(false);
  let stopListening: (() => void) | null = null;

  function beginLiveRun(steps: InstallStep[]) {
    liveSteps = steps.map((step) => ({
      id: step.id,
      description: step.description,
      status: null,
      detail: null,
      started: false,
    }));
    liveCurrent = null;
    liveLog = [];
    overlayOpen = true;

    stopListening?.();
    stopListening = onDaemonEvent((event) => {
      if (event.topic !== "installer.progress") return;
      const progress = event.payload as unknown as InstallerProgress;
      // A second window, or a `pyren-ctl` run, could be installing too.
      // Only this panel's own action belongs on this panel.
      if (progress.action !== action) return;

      const step = liveSteps[progress.index];
      if (!step) return;

      if (progress.status === null) {
        step.started = true;
        liveCurrent = progress.index;
        liveLog = [...liveLog, `> ${tm(progress.description)}`];
        return;
      }

      step.status = progress.status;
      step.detail = progress.detail;
      if (liveCurrent === progress.index) liveCurrent = null;
      const detail = progress.detail ? tm(progress.detail) : "";
      liveLog = [...liveLog, `  [${progress.status}]${detail ? ` ${detail}` : ""}`];
    });
  }

  function endLiveRun() {
    stopListening?.();
    stopListening = null;
    liveCurrent = null;
  }

  function closeOverlay() {
    overlayOpen = false;
  }

  onDestroy(() => stopListening?.());

  /**
   * The options a dry run was last read for. Anything typed after that
   * invalidates it: an "Apply" that ran a plan the user never saw would be
   * exactly the button this page exists to avoid.
   */
  let dryRunKey = $state<string | null>(null);

  const env = $derived(inspection?.environment ?? null);

  const boardTable = $derived.by<BoardTable | null>(() => {
    if (!experimentalBoard.trim()) return null;
    return boardTableName === "features"
      ? { table: "features", params: boardParams }
      : ({ table: boardTableName } as BoardTable);
  });

  /** Blank and a non-number both mean "leave the driver's own fallback". */
  function rpm(value: string): number | null {
    const n = Number(value.trim());
    return value.trim() !== "" && Number.isFinite(n) && n > 0 ? Math.round(n) : null;
  }

  // In automatic mode the manual fields are not on screen, so they are not
  // sent either: a value the user cannot see must not decide what runs.
  const request = $derived.by<InstallerRequest>(() => ({
    action,
    auto,
    preferHooks,
    force,
    cpuMaxRpm: auto ? null : rpm(cpuMaxRpm),
    gpuMaxRpm: auto ? null : rpm(gpuMaxRpm),
    experimentalBoard: auto ? null : experimentalBoard.trim() || null,
    boardTable: auto ? null : boardTable,
    skipSteps,
  }));

  const optionsKey = $derived(JSON.stringify(request));

  /**
   * The subset of the request that decides *what the steps are*. Planning
   * is pure over these three; everything else only decides what the steps
   * are given to do, or whether one runs at all.
   *
   * Kept apart from `optionsKey` so that unticking an optional step does
   * not delete the very list the checkboxes live in. It still throws the
   * dry run away, which is the property that matters: the report on screen
   * must always be the report for what would now run.
   */
  const planKey = $derived(JSON.stringify({ action, preferHooks, force }));

  /** A driver action only; the service is installed by the panel above. */
  const isDriverAction = $derived(
    action === "installDriver" || action === "restoreDriver" || action === "pinFanCeiling",
  );

  const runnable = $derived(!!plan && plan.blockers.length === 0 && plan.steps.length > 0);
  const dryRunCurrent = $derived(dryRunKey === optionsKey);

  /**
   * `patchNeeded` alone is two answers wearing one flag. A machine where
   * `pwm1` is missing *and* the kernel has manual fan control of its own
   * reports `false` — the patch is not the standard remedy — but fan
   * control still does not work, and the stock driver coming up without
   * `pwm1` is precisely the "your board is not in its tables" case the
   * board fields below exist for. Calling that "you do not need this"
   * would send the one user who does need it away.
   */
  const verdict = $derived.by<"needed" | "works" | "boardMissing" | null>(() => {
    if (!inspection || !env) return null;
    if (inspection.patchNeeded) return "needed";
    return env.fanControlAvailable ? "works" : "boardMissing";
  });

  /** Calibrated ceiling, if `fan.calibrate` has ever been run here. */
  const measuredMaxRpm = $derived(hardware.fan?.fanMaxRpm ?? null);

  /**
   * Whether to measure the ceiling once the install is done.
   *
   * Defaults to on for a machine that has never been calibrated, which is
   * every machine at its first install - that is the case where nobody
   * comes back to do it, and the driver is left scaling pwm against a
   * guess. A machine that already has a measurement gets it off: twenty
   * seconds of full-speed fans to re-learn a number it already knows is
   * a poor default. It is a checkbox either way, because the fans are
   * loud and a surprise is worse than a wait.
   */
  let calibrateAfter = $state(false);
  let calibrateTouched = $state(false);
  $effect(() => {
    if (!calibrateTouched) calibrateAfter = measuredMaxRpm === null;
  });

  async function inspect() {
    inspecting = true;
    inspectError = null;
    try {
      inspection = await daemon.installerInspect();
    } catch (e) {
      inspectError = String(e);
      inspection = null;
    } finally {
      inspecting = false;
    }
  }

  function toggleOpen() {
    open = !open;
    if (open && !inspection && !inspecting) inspect();
  }

  // A different set of steps means the plan on screen is not this plan.
  $effect(() => {
    planKey;
    plan = null;
    planError = null;
    // Steps unticked for a plan that no longer exists mean nothing, and
    // could silently apply to a step of the same name in the next one.
    skipSteps = [];
  });

  // Any change at all disarms the apply: the report on screen must always
  // be the report for exactly what would run now.
  $effect(() => {
    optionsKey;
    report = null;
    runError = null;
    dryRunKey = null;
  });

  function toggleStep(id: string, run: boolean) {
    skipSteps = run ? skipSteps.filter((s) => s !== id) : [...skipSteps, id];
  }

  async function review() {
    planning = true;
    planError = null;
    report = null;
    dryRunKey = null;
    try {
      plan = await daemon.installerPlan(request);
    } catch (e) {
      planError = String(e);
      plan = null;
    } finally {
      planning = false;
    }
  }

  /**
   * The install button: survey the machine, then dry-run the plan that
   * survey produces, in one click.
   *
   * The `tick()` matters. Setting `auto` changes the request, and the
   * effect above throws away any plan on screen when that happens - so the
   * dry run has to be started *after* that has settled, or it would arm an
   * apply that the effect then immediately disarms.
   */
  async function installAutomatically() {
    action = "installDriver";
    mode = "automatic";
    await tick();

    detecting = true;
    autoError = null;
    try {
      // Probing reads the embedded controller to settle the board-params
      // variant instead of guessing it, which means loading `ec_sys`
      // (read-only). Clicking install is what authorises that; the plain
      // call stays a pure read.
      detected = await daemon.installerAutodetect(true);
    } catch (e) {
      autoError = String(e);
      detected = null;
      detecting = false;
      return;
    }
    detecting = false;
    await run(false);
  }

  /**
   * Measuring the ceiling, as the last phase of the install.
   *
   * It has to be here rather than on the fan page, because of an ordering
   * nothing else can get around: measuring means running the fans at full
   * speed, which needs `pwm1`, which on these boards needs the driver that
   * was *just* installed. So the first install on any machine is always
   * made before a measurement exists - and until now nothing ever went
   * back, which left every driver converting pwm to rpm against a number
   * it had guessed. The daemon writes the result where the driver reads
   * it, so this is the moment the whole chain closes.
   */
  async function calibratePhase() {
    const step: LiveStep = {
      id: "calibrate",
      description: {
        key: "install.step.calibrate",
        text: "Measure what full speed is on this machine",
      },
      status: null,
      detail: null,
      started: true,
    };
    liveSteps = [...liveSteps, step];
    liveCurrent = liveSteps.length - 1;
    liveLog = [...liveLog, `> ${tm(step.description)}`, `  ${t("install.calibrateLoud")}`];

    // The call blocks until the fans settle, so the only way to show the
    // ramp as it happens is to watch the same tachometer it is watching.
    const watch = setInterval(async () => {
      try {
        const status = await daemon.fanStatus();
        if (status.fanRpm) liveLog = [...liveLog, `  ${status.fanRpm} rpm`];
      } catch {
        /* A reading that did not arrive is not worth failing a run over. */
      }
    }, 1000);

    try {
      const calibration = await daemon.calibrateFans(30);
      const measured = calibration.verdict === "measured";
      step.status = measured ? "ok" : "warned";
      step.detail = { key: "", text: calibration.detail };
      liveLog = [...liveLog, `  [${step.status}] ${calibration.detail}`];
      if (calibration.pinned) {
        liveLog = [...liveLog, `  ${calibration.pinned.detail}`];
      }
    } catch (e) {
      step.status = "failed";
      step.detail = { key: "", text: String(e) };
      liveLog = [...liveLog, `  [failed] ${String(e)}`];
    } finally {
      clearInterval(watch);
      liveCurrent = null;
      liveSteps = [...liveSteps];
    }
  }

  /**
   * The overlay covers the window, so it must not be up while the machine
   * is only being *asked* about. It appears for a run and stays until the
   * report has been read.
   */
  const overlayTitle = $derived(
    t(liveConfirm ? `install.title.${action}` : "install.title.dryRun"),
  );

  async function run(confirm: boolean) {
    running = true;
    runError = null;
    const key = optionsKey;
    // The steps are known before the call: they are the plan's, and the
    // daemon runs them in order. Showing them up front, greyed, is what
    // makes the panel a progress bar rather than a log that grows out of
    // nothing.
    //
    // No plan means automatic mode's opening dry run, which is the one
    // call here that has nothing to draw yet - and nothing to wait for
    // either, since a rehearsal runs no commands. The panel it would put
    // up would be an empty bar over a window that was never blocked.
    liveConfirm = confirm;
    if (plan) beginLiveRun(plan.steps);
    try {
      const result = await daemon.installerApply({ ...request, confirm });
      plan = result.plan;
      report = result.report;
      // What the daemon actually filled in, which is the authority - the
      // survey shown on screen was a separate call and could be older.
      if (result.autodetected) detected = result.autodetected;
      // Only a dry run arms the real one, and only for these options.
      dryRunKey = confirm ? null : key;
      if (confirm) {
        // What was just installed changes every answer on this panel.
        await inspect();
        if (result.report.succeeded && action === "installDriver" && calibrateAfter) {
          await calibratePhase();
        }
      }
    } catch (e) {
      runError = String(e);
    } finally {
      running = false;
      endLiveRun();
    }
  }

  const stepIcons: Record<StepStatus, string> = {
    ok: "check",
    warned: "warning",
    failed: "close",
    skipped: "info",
    declined: "info",
    planned: "info",
  };

  const boardTables: BoardTable["table"][] = [
    "features",
    "omenThermalProfile",
    "omenForceV0",
    "omenTimed",
    "victusThermalProfile",
  ];

  const boardParamsOptions: BoardParams[] = [
    "victusS",
    "omenV1",
    "omenV1Legacy",
    "omenV1NoEc",
  ];
</script>

<InstallProgress
  open={overlayOpen}
  title={overlayTitle}
  steps={liveSteps}
  current={liveCurrent}
  log={liveLog}
  finished={!running}
  succeeded={!!report?.succeeded && !runError}
  error={runError}
  onclose={closeOverlay}
/>

<Panel>
  <button class="disclosure" onclick={toggleOpen} aria-expanded={open}>
    <Icon name={open ? "chevronDown" : "chevronRight"} size={15} />
    <span class="disclosure-title">{t("installer.title")}</span>
    <span class="disclosure-note">{t("installer.advanced")}</span>
  </button>

  {#if open}
    <p class="hint">{t("installer.intro")}</p>

    {#if inspectError}
      <p class="notice err">{inspectError}</p>
    {:else if !inspection}
      <p class="notice">{inspecting ? t("installer.inspecting") : t("common.loading")}</p>
    {/if}

    {#if env && inspection}
      <!-- The verdict first: on most machines it is "you do not need
           this", and that has to be the first thing read, not a footnote
           under an install button. -->
      <p class="verdict {verdict}">
        <Icon name={verdict === "works" ? "check" : "warning"} size={16} />
        <span>{t(`installer.verdict.${verdict}`)}</span>
      </p>

      <dl class="facts">
        <div><dt>{t("installer.kernel")}</dt><dd>{env.kernel.release}</dd></div>
        <div>
          <dt>{t("installer.upstream")}</dt>
          <dd>{env.kernel.hasUpstreamFanControl ? t("common.on") : t("common.off")}</dd>
        </div>
        <div>
          <dt>{t("installer.pwm")}</dt>
          <dd>{env.fanControlAvailable ? t("common.on") : t("common.off")}</dd>
        </div>
        <div>
          <dt>{t("installer.source")}</dt>
          <dd class:missing={!env.driverSource}>
            {env.driverSource ?? t("installer.sourceMissing")}
          </dd>
        </div>
        <div>
          <dt>{t("installer.headers")}</dt>
          <dd class:missing={!env.headers.usable}>
            {env.headers.usable
              ? (env.headers.buildDir ?? t("common.on"))
              : (env.headers.fixHint ?? t("installer.headersMissing"))}
          </dd>
        </div>
        <div>
          <dt>{t("installer.strategyLabel")}</dt>
          <dd>
            {env.hasDkms ? "dkms" : env.hookFlavour}
            {#if env.dkmsStatus}
              <span class="sub">{env.dkmsStatus}</span>
            {/if}
          </dd>
        </div>
        <div>
          <dt>{t("installer.initramfs")}</dt>
          <dd>{env.initramfsTool ?? t("common.unavailable")}</dd>
        </div>
        <div>
          <dt>{t("installer.service")}</dt>
          <dd>{env.serviceInstalled ? t("drivers.installed") : t("drivers.notInstalled")}</dd>
        </div>
      </dl>

      <div class="field">
        <span class="label">{t("installer.action")}</span>
        <Segmented
          options={[
            { value: "installDriver", label: t("installer.actions.installDriver") },
            { value: "restoreDriver", label: t("installer.actions.restoreDriver") },
          ]}
          value={action}
          onchange={(v) => (action = v as InstallerAction)}
        />
      </div>

      {#if action === "installDriver"}
        <!-- The two ways in, as a choice rather than a button sitting above
             a form. Automatic asks the machine what it is; manual is for
             someone who already knows and disagrees. -->
        <div class="field">
          <span class="label">{t("installer.mode")}</span>
          <Segmented
            options={[
              { value: "automatic", label: t("installer.modes.automatic") },
              { value: "manual", label: t("installer.modes.manual") },
            ]}
            value={mode}
            onchange={(v) => (mode = v as InstallMode)}
          />
        </div>

        <p class="hint">
          {auto ? t("installer.autoHint") : t("installer.manualHint")}
        </p>

        {#if auto}
          {#if autoError}<p class="notice err">{autoError}</p>{/if}

          {#if detected}
            <dl class="facts">
              <div>
                <dt>{t("installer.detectedBoard")}</dt>
                <dd>{detected.dmi.boardName ?? t("common.unavailable")}</dd>
              </div>
              <div>
                <dt>{t("installer.detectedModel")}</dt>
                <dd>{detected.dmi.productName ?? t("common.unavailable")}</dd>
              </div>
              <div>
                <dt>{t("installer.detectedTable")}</dt>
                <dd>
                  {#if detected.boardKnown}
                    {t("installer.detectedNoPatch")}
                  {:else if detected.boardTable}
                    {t(`installer.tables.${detected.boardTable.table}`)}
                    {detected.boardTable.table === "features"
                      ? ` — ${t(`installer.params.${detected.boardTable.params}`)}`
                      : ""}
                  {:else}
                    {t("installer.detectedUndecided")}
                  {/if}
                </dd>
              </div>
              <div>
                <dt>{t("installer.detectedCeilings")}</dt>
                <dd>
                  {detected.cpuMaxRpm || detected.gpuMaxRpm
                    ? t("installer.detectedCeilingsValue", {
                        cpu: String(detected.cpuMaxRpm ?? "—"),
                        gpu: String(detected.gpuMaxRpm ?? "—"),
                      })
                    : t("installer.driverDefault")}
                </dd>
              </div>
            </dl>

            <!-- Why each answer is what it is. The point of showing this at
                 all: a filled-in form presented as fact would be worse than
                 the questions it replaced. -->
            <ul class="list">
              {#each detected.notes as note, i (i)}
                <li>
                  <Icon name="info" size={15} />
                  <div class="body"><span class="check-title">{tm(note)}</span></div>
                </li>
              {/each}
            </ul>
          {/if}
        {:else}
          <div class="options">
            <div class="rpm">
              <span class="label">{t("installer.maxRpm")}</span>
              <em class="sub">
                {measuredMaxRpm
                  ? t("installer.maxRpmMeasured", { rpm: String(measuredMaxRpm) })
                  : t("installer.maxRpmHint")}
              </em>
              <div class="row">
                <label>
                  <span>{t("installer.cpuMaxRpm")}</span>
                  <input
                    type="text"
                    inputmode="numeric"
                    bind:value={cpuMaxRpm}
                    placeholder={t("installer.driverDefault")}
                  />
                </label>
                <label>
                  <span>{t("installer.gpuMaxRpm")}</span>
                  <input
                    type="text"
                    inputmode="numeric"
                    bind:value={gpuMaxRpm}
                    placeholder={t("installer.driverDefault")}
                  />
                </label>
                {#if measuredMaxRpm}
                  <button class="ghost" onclick={() => (cpuMaxRpm = String(measuredMaxRpm))}>
                    {t("installer.useMeasured")}
                  </button>
                {/if}
              </div>
            </div>

            <div class="rpm">
              <span class="label">{t("installer.board")}</span>
              <em class="sub">{t("installer.boardHint")}</em>
              <div class="row">
                <label>
                  <span>{t("installer.boardId")}</span>
                  <input
                    type="text"
                    bind:value={experimentalBoard}
                    placeholder="8D2F"
                    spellcheck="false"
                  />
                </label>
                <label>
                  <span>{t("installer.boardTable")}</span>
                  <select bind:value={boardTableName} disabled={!experimentalBoard.trim()}>
                    {#each boardTables as name (name)}
                      <option value={name}>{t(`installer.tables.${name}`)}</option>
                    {/each}
                  </select>
                </label>
                {#if boardTableName === "features"}
                  <label>
                    <span>{t("installer.boardParams")}</span>
                    <select bind:value={boardParams} disabled={!experimentalBoard.trim()}>
                      {#each boardParamsOptions as name (name)}
                        <option value={name}>{t(`installer.params.${name}`)}</option>
                      {/each}
                    </select>
                  </label>
                {/if}
              </div>
            </div>
          </div>
        {/if}

        <!-- Both modes: how the module survives a kernel upgrade, and
             whether to proceed where installing is questionable. -->
        <div class="options">
          <label class="switch">
            <Toggle
              checked={preferHooks}
              onchange={(v) => (preferHooks = v)}
              ariaLabel={t("installer.preferHooks")}
            />
            <span>
              <strong>{t("installer.preferHooks")}</strong>
              <em>{t("installer.preferHooksHint")}</em>
            </span>
          </label>

          <label class="switch">
            <Toggle checked={force} onchange={(v) => (force = v)} ariaLabel={t("installer.force")} />
            <span>
              <strong>{t("installer.force")}</strong>
              <em>{t("installer.forceHint")}</em>
            </span>
          </label>
        </div>
      {/if}

      <!-- Asked before the install, not after, because the answer is
           twenty seconds of full-speed fans. On a machine with no
           measurement yet it starts on: that machine cannot have been
           calibrated before the driver existed, and nobody comes back. -->
      {#if action === "installDriver"}
        <div class="options">
          <label class="switch">
            <Toggle
              checked={calibrateAfter}
              onchange={(v) => {
                calibrateAfter = v;
                calibrateTouched = true;
              }}
              ariaLabel={t("install.calibrateAfter")}
            />
            <span>
              <strong>{t("install.calibrateAfter")}</strong>
              <em>{measuredMaxRpm ? t("install.calibrateAgainHint", { rpm: String(measuredMaxRpm) }) : t("install.calibrateFirstHint")}</em>
            </span>
          </label>
        </div>
      {/if}

      <!-- One row of buttons; the mode decides what is in it. Confirming is
           a separate click in both modes, and stays unreachable until a dry
           run of these exact options has come back. -->
      <div class="actions">
        {#if auto}
          <button
            class="run"
            onclick={installAutomatically}
            disabled={detecting || planning || running}
          >
            <Icon name="search" size={15} />
            {detecting
              ? t("installer.autoWorking")
              : running && !report
                ? t("installer.running")
                : t("installer.autoButton")}
          </button>
        {:else}
          <button class="run" onclick={review} disabled={planning || running}>
            <Icon name="search" size={15} />
            {planning ? t("installer.reviewing") : t("installer.review")}
          </button>

          <button
            class="run"
            onclick={() => run(false)}
            disabled={!runnable || planning || running}
          >
            <Icon name="refresh" size={15} />
            {running && !report ? t("installer.running") : t("installer.dryRun")}
          </button>
        {/if}

        <button
          class="run danger"
          onclick={() => run(true)}
          disabled={!runnable || !dryRunCurrent || running}
        >
          <Icon name="download" size={15} />
          {auto ? t("installer.applyAuto") : t("installer.apply")}
        </button>
      </div>

      {#if plan && !dryRunCurrent && runnable}
        <p class="hint">{t("installer.dryRunFirst")}</p>
      {/if}

      {#if planError}<p class="notice err">{planError}</p>{/if}
      {#if runError}<p class="notice err">{runError}</p>{/if}

      {#if plan}
        {#if plan.blockers.length > 0}
          <div class="block">
            <h3>{t("installer.blockers")}</h3>
            <ul class="list">
              {#each plan.blockers as blocker (blocker.id)}
                <li class="fail">
                  <Icon name="close" size={15} />
                  <div class="body">
                    <span class="check-title">{tm(blocker.message)}</span>
                    {#if blocker.fix}<code>{blocker.fix}</code>{/if}
                  </div>
                </li>
              {/each}
            </ul>
          </div>
        {/if}

        {#if plan.warnings.length > 0}
          <div class="block">
            <h3>{t("installer.warnings")}</h3>
            <ul class="list">
              {#each plan.warnings as warning, i (i)}
                <li class="warn">
                  <Icon name="warning" size={15} />
                  <div class="body"><span class="check-title">{tm(warning)}</span></div>
                </li>
              {/each}
            </ul>
          </div>
        {/if}

        {#if plan.steps.length > 0}
          <div class="block">
            <h3>
              {t("installer.steps", { count: String(plan.steps.length) })}
              {#if plan.strategy}
                <span class="tag">{plan.strategy}</span>
              {/if}
              {#if plan.needsRoot}
                <span class="tag">{t("installer.needsRoot")}</span>
              {/if}
            </h3>
            {#if plan.steps.some((step) => step.optional)}
              <p class="hint step-hint">{t("installer.stepsHint")}</p>
            {/if}
            <ol class="steps">
              {#each plan.steps as step (step.id)}
                {@const declined = skipSteps.includes(step.id)}
                <li class:declined>
                  <span class="check-title">
                    <!-- Only optional steps get a switch. A required one has
                         no checkbox at all rather than a disabled one: the
                         daemon refuses to skip it, so offering the control
                         would be offering something that cannot happen. -->
                    {#if step.optional}
                      <Toggle
                        checked={!declined}
                        onchange={(v) => toggleStep(step.id, v)}
                        ariaLabel={tm(step.description)}
                      />
                    {/if}
                    {tm(step.description)}
                    {#if step.optional}
                      <span class="tag">
                        {declined ? t("installer.willSkip") : t("installer.optional")}
                      </span>
                    {/if}
                  </span>
                  <code>
                    {step.command.length > 0
                      ? step.command.join(" ")
                      : t("installer.internalStep")}
                  </code>
                </li>
              {/each}
            </ol>
          </div>
        {/if}
      {/if}

      {#if report}
        <div class="block">
          <h3>
            {report.dryRun ? t("installer.dryRunReport") : t("installer.report")}
            <span class="tag" class:ok={report.succeeded} class:bad={!report.succeeded}>
              {report.succeeded ? t("installer.succeeded") : t("installer.failed")}
            </span>
          </h3>
          <ul class="list">
            {#each report.results as result (result.id)}
              <li class={result.status}>
                <Icon name={stepIcons[result.status]} size={15} />
                <div class="body">
                  <span class="check-title">{tm(result.description)}</span>
                  {#if result.detail}<span class="detail">{tm(result.detail)}</span>{/if}
                </div>
              </li>
            {/each}
          </ul>
          <!-- What to do next, decided by looking rather than guessing.
               `inspect` re-runs after a real install, and pwm1's presence
               is the definitive test of whether the new module is the one
               in use - the plan's own modprobe usually does the reload, so
               telling everyone to reboot was asking for something already
               done. -->
          {#if !report.dryRun && report.succeeded && isDriverAction}
            {#if action === "installDriver" && env?.fanControlAvailable}
              <!-- This used to end with "now run `sudo systemctl restart
                   pyren-daemon`", because installing reloads hp-wmi, which
                   renumbers the hwmon directory the fan module found at
                   startup - so it kept reading a directory that no longer
                   existed and reported that no speed could be set. The
                   daemon looks again by itself now (`FanModule::rediscover`,
                   wired to the installer in `daemon/src/main.rs`), so there
                   is nothing left to ask for. -->
              <p class="notice ok-notice">{t("installer.afterInstall.working")}</p>
            {:else if action === "installDriver"}
              <p class="notice warn">{t("installer.afterInstall.needsReload")}</p>
              <code class="fix">sudo modprobe -r hp-wmi &amp;&amp; sudo modprobe hp-wmi</code>
              <p class="hint">{t("installer.afterInstall.ifStillNothing")}</p>
            {:else}
              <p class="notice warn">{t("installer.afterInstall.restored")}</p>
              <code class="fix">sudo modprobe -r hp-wmi &amp;&amp; sudo modprobe hp-wmi</code>
            {/if}
          {/if}
        </div>
      {/if}
    {/if}
  {/if}
</Panel>

<style>
  .disclosure {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    padding: 0;
    border: none;
    background: transparent;
    color: var(--text);
    text-align: left;
  }

  .disclosure-title {
    font-size: 15px;
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }

  .disclosure-note {
    margin-left: auto;
    font-size: 11px;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--text-mute);
  }

  .hint {
    margin: 14px 0 0;
    color: var(--text-mute);
    font-size: 12px;
    line-height: 1.55;
  }

  .notice {
    margin: 14px 0 0;
    font-size: 13px;
    line-height: 1.5;
    color: var(--text-dim);
  }

  .notice.err {
    color: var(--danger);
  }

  .notice.warn {
    color: var(--warn);
  }

  .notice.ok-notice {
    color: var(--ok);
  }

  /* A command to copy, not one the installer is about to run. */
  .fix {
    display: block;
    margin: 8px 0 0;
    padding: 8px 10px;
    border-radius: 2px;
    background: var(--bg-elev, rgba(255, 255, 255, 0.04));
    color: var(--text-dim);
    font-size: 12.5px;
    user-select: text;
  }

  .verdict {
    display: flex;
    gap: 10px;
    align-items: flex-start;
    margin: 16px 0 0;
    font-size: 13.5px;
    line-height: 1.5;
  }

  .verdict.needed,
  .verdict.boardMissing {
    color: var(--warn);
  }

  .verdict.works {
    color: var(--ok);
  }

  .facts {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
    gap: 8px 22px;
    margin: 16px 0 0;
  }

  .facts div {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 6px 0;
    border-bottom: 1px solid var(--line-soft);
    font-size: 12.5px;
  }

  dt {
    color: var(--text-mute);
    white-space: nowrap;
  }

  dd {
    margin: 0;
    color: var(--text-dim);
    text-align: left;
    overflow-wrap: anywhere;
  }

  dd .sub {
    display: block;
    color: var(--text-mute);
    font-size: 11.5px;
  }

  dd.missing {
    color: var(--warn);
  }

  .field {
    display: flex;
    align-items: center;
    gap: 16px;
    margin: 20px 0 0;
    flex-wrap: wrap;
  }

  .label {
    font-size: 11px;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--text-mute);
  }

  .options {
    display: flex;
    flex-direction: column;
    gap: 16px;
    margin: 18px 0 0;
  }

  .switch {
    display: flex;
    align-items: flex-start;
    gap: 12px;
  }

  .switch strong {
    display: block;
    font-size: 13px;
    font-weight: 500;
  }

  em,
  .detail {
    display: block;
    color: var(--text-mute);
    font-size: 12px;
    font-style: normal;
    line-height: 1.45;
  }

  .rpm .sub {
    margin: 4px 0 8px;
  }

  .row {
    display: flex;
    align-items: flex-end;
    gap: 14px;
    flex-wrap: wrap;
  }

  .row label {
    display: flex;
    flex-direction: column;
    gap: 5px;
    font-size: 12px;
    color: var(--text-dim);
  }

  input,
  select {
    width: 160px;
    padding: 7px 9px;
    border: 1px solid var(--line-soft);
    border-radius: 2px;
    background: var(--bg-elev, transparent);
    color: var(--text);
    font-size: 13px;
  }

  input:disabled,
  select:disabled {
    opacity: 0.45;
  }


  .actions {
    display: flex;
    gap: 12px;
    margin: 22px 0 0;
    flex-wrap: wrap;
  }

  .run {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    padding: 9px 20px;
    border: none;
    border-radius: 2px;
    background: #f2f2f4;
    color: #17171a;
    font-size: 12px;
    font-weight: 700;
    letter-spacing: 0.05em;
    text-transform: uppercase;
  }

  .run.danger {
    background: transparent;
    border: 1px solid var(--danger);
    color: var(--danger);
  }

  .run:disabled {
    opacity: 0.45;
  }

  .ghost {
    padding: 7px 14px;
    border: 1px solid var(--accent-2);
    border-radius: 2px;
    background: transparent;
    color: var(--text);
    font-size: 11px;
    letter-spacing: 0.05em;
    text-transform: uppercase;
  }

  .block {
    margin: 22px 0 0;
  }

  h3 {
    display: flex;
    align-items: center;
    gap: 10px;
    font-size: 11px;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--text-mute);
    margin: 0 0 8px;
  }

  .tag {
    padding: 2px 7px;
    border: 1px solid var(--line-soft);
    border-radius: 2px;
    font-size: 10px;
    letter-spacing: 0.05em;
    color: var(--text-dim);
  }

  .tag.ok {
    color: var(--ok);
    border-color: var(--ok);
  }

  .tag.bad {
    color: var(--danger);
    border-color: var(--danger);
  }

  .list,
  .steps {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
  }

  .list li {
    display: flex;
    gap: 12px;
    align-items: flex-start;
    padding: 9px 0;
    border-bottom: 1px solid var(--line-soft);
  }

  .steps li {
    display: flex;
    flex-direction: column;
    gap: 5px;
    padding: 9px 0;
    border-bottom: 1px solid var(--line-soft);
  }

  .list li:last-child,
  .steps li:last-child {
    border-bottom: none;
  }

  .list li.ok {
    color: var(--ok);
  }
  .list li.failed,
  .list li.fail {
    color: var(--danger);
  }
  .list li.warned,
  .list li.warn {
    color: var(--warn);
  }
  .list li.skipped,
  .list li.declined,
  .list li.planned {
    color: var(--text-mute);
  }

  .step-hint {
    margin: 0 0 10px;
  }

  /* Unticked in the plan: still listed, visibly not going to happen. */
  .steps li.declined .check-title,
  .steps li.declined code {
    opacity: 0.5;
  }

  .body {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .check-title {
    display: flex;
    align-items: center;
    gap: 8px;
    color: var(--text);
    font-size: 13.5px;
    line-height: 1.4;
  }

  code {
    display: block;
    font-family: var(--mono, ui-monospace, monospace);
    font-size: 12px;
    color: var(--text-dim);
    background: var(--bg-deep, rgba(255, 255, 255, 0.04));
    padding: 5px 8px;
    border-radius: 2px;
    overflow-x: auto;
    white-space: pre-wrap;
    word-break: break-all;
  }
</style>
