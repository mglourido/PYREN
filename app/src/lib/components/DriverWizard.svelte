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
   */
  import Icon from "$lib/components/Icon.svelte";
  import Panel from "$lib/components/Panel.svelte";
  import Toggle from "$lib/components/Toggle.svelte";
  import Segmented from "$lib/components/Segmented.svelte";
  import {
    daemon,
    type BoardParams,
    type BoardTable,
    type ExecutionReport,
    type InstallerAction,
    type InstallerInspection,
    type InstallerRequest,
    type InstallPlan,
    type StepStatus,
  } from "$lib/api/daemon";
  import { hardware } from "$lib/stores/hardware.svelte";
  import { t } from "$lib/i18n/index.svelte";

  let open = $state(false);

  let inspection = $state<InstallerInspection | null>(null);
  let inspecting = $state(false);
  let inspectError = $state<string | null>(null);

  let action = $state<InstallerAction>("installDriver");
  let preferHooks = $state(false);
  let force = $state(false);
  let cpuMaxRpm = $state<string>("");
  let gpuMaxRpm = $state<string>("");
  let experimentalBoard = $state<string>("");
  let boardTableName = $state<BoardTable["table"]>("features");
  let boardParams = $state<BoardParams>("victusS");

  let plan = $state<InstallPlan | null>(null);
  let planning = $state(false);
  let planError = $state<string | null>(null);

  let report = $state<ExecutionReport | null>(null);
  let running = $state(false);
  let runError = $state<string | null>(null);

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

  const request = $derived.by<InstallerRequest>(() => ({
    action,
    preferHooks,
    force,
    cpuMaxRpm: rpm(cpuMaxRpm),
    gpuMaxRpm: rpm(gpuMaxRpm),
    experimentalBoard: experimentalBoard.trim() || null,
    boardTable,
  }));

  const optionsKey = $derived(JSON.stringify(request));

  /** A driver action only; the service is installed by the panel above. */
  const isDriverAction = $derived(action === "installDriver" || action === "restoreDriver");

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

  // Any change to the request invalidates what is on screen, rather than
  // leaving a plan next to options that no longer produced it.
  $effect(() => {
    optionsKey;
    plan = null;
    report = null;
    planError = null;
    runError = null;
    dryRunKey = null;
  });

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

  async function run(confirm: boolean) {
    running = true;
    runError = null;
    const key = optionsKey;
    try {
      const result = await daemon.installerApply({ ...request, confirm });
      plan = result.plan;
      report = result.report;
      // Only a dry run arms the real one, and only for these options.
      dryRunKey = confirm ? null : key;
      if (confirm) {
        // What was just installed changes every answer on this panel.
        await inspect();
      }
    } catch (e) {
      runError = String(e);
    } finally {
      running = false;
    }
  }

  const stepIcons: Record<StepStatus, string> = {
    ok: "check",
    warned: "warning",
    failed: "close",
    skipped: "info",
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
            {env.dkmsStatus ? ` — ${env.dkmsStatus}` : ""}
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
            <Toggle
              checked={force}
              onchange={(v) => (force = v)}
              ariaLabel={t("installer.force")}
            />
            <span>
              <strong>{t("installer.force")}</strong>
              <em>{t("installer.forceHint")}</em>
            </span>
          </label>

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
                <button
                  class="ghost"
                  onclick={() => (cpuMaxRpm = String(measuredMaxRpm))}
                >
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

      <div class="actions">
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

        <button
          class="run danger"
          onclick={() => run(true)}
          disabled={!runnable || !dryRunCurrent || running}
        >
          <Icon name="download" size={15} />
          {t("installer.apply")}
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
                    <span class="check-title">{blocker.message}</span>
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
                  <div class="body"><span class="check-title">{warning}</span></div>
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
            <ol class="steps">
              {#each plan.steps as step (step.id)}
                <li>
                  <span class="check-title">
                    {step.description}
                    {#if step.optional}<span class="tag">{t("installer.optional")}</span>{/if}
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
                  <span class="check-title">{result.description}</span>
                  {#if result.detail}<span class="detail">{result.detail}</span>{/if}
                </div>
              </li>
            {/each}
          </ul>
          {#if !report.dryRun && report.succeeded && isDriverAction}
            <p class="notice warn">{t("installer.rebootHint")}</p>
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
    gap: 10px;
    justify-content: space-between;
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
    text-align: right;
    word-break: break-all;
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
  .list li.planned {
    color: var(--text-mute);
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
