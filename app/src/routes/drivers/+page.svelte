<script lang="ts">
  /**
   * Hardware check.
   *
   * This page used to be an installer. It is a verifier instead: manual fan
   * control is upstream in recent kernels, so on most machines the right
   * answer is "the stock driver already does this" and the useful thing is
   * to prove it - and, when it doesn't work, to say precisely which part is
   * missing rather than offering to replace a kernel module.
   */
  import DriverWizard from "$lib/components/DriverWizard.svelte";
  import Icon from "$lib/components/Icon.svelte";
  import InfoTip from "$lib/components/InfoTip.svelte";
  import Panel from "$lib/components/Panel.svelte";
  import Toggle from "$lib/components/Toggle.svelte";
  import { daemon, errorText, type FanDiagnosis, type CheckStatus } from "$lib/api/daemon";
  import { admin, type AdminAction, type AdminStatus } from "$lib/api/admin";
  import { t, tm } from "$lib/i18n/index.svelte";
  import { telemetry } from "$lib/stores/telemetry.svelte";
  import { onMount } from "svelte";

  let diagnosis = $state<FanDiagnosis | null>(null);
  let running = $state(false);
  let error = $state<string | null>(null);
  let allowWrites = $state(false);

  async function run() {
    running = true;
    error = null;
    try {
      diagnosis = await daemon.fanDiagnose(allowWrites);
    } catch (e) {
      error = errorText(e);
      diagnosis = null;
    } finally {
      running = false;
    }
  }

  /**
   * Third check alongside the fan diagnosis and the speed probe: does the
   * keyboard lighting hardware answer at all, rather than trusting whatever
   * state the Lighting page last showed.
   */
  let rgbChecking = $state(false);
  let rgbResult = $state<{ ok: boolean; text: string } | null>(null);

  async function checkRgbKeyboard() {
    rgbChecking = true;
    rgbResult = null;
    try {
      const probe = await daemon.rgbCapabilities();
      rgbResult =
        probe.lighting.present || probe.perKey.present
          ? { ok: true, text: t("diagnostics.rgbPresent") }
          : probe.lighting.unreachable
            ? { ok: false, text: tm(probe.lighting.unreachable) }
            : { ok: false, text: t("diagnostics.rgbAbsent") };
    } catch (e) {
      rgbResult = { ok: false, text: errorText(e) };
    } finally {
      rgbChecking = false;
    }
  }

  // --- Admin mode --------------------------------------------------------
  //
  // Most "Pyren is broken" is missing privilege, and from the UI the two
  // look identical: an unreachable daemon and unsupported hardware both
  // show the same demo numbers. These rows say which it is.

  let privileges = $state<AdminStatus | null>(null);
  let granting = $state<AdminAction | null>(null);
  let grantError = $state<string | null>(null);
  let statusError = $state<string | null>(null);
  let reloginNeeded = $state(false);

  /** In a browser there is no shell to ask, so the panel isn't shown. */
  const canInspect = admin.available();

  async function refreshPrivileges() {
    if (!canInspect) return;
    try {
      privileges = await admin.status();
      statusError = null;
    } catch (e) {
      // Kept apart from grantError: failing to *read* the state and failing
      // to *change* it are different problems with different remedies.
      statusError = errorText(e);
    }
  }

  async function applyGrant(action: AdminAction) {
    granting = action;
    grantError = null;
    try {
      const result = await admin.grant(action);
      // A dismissed polkit dialog is a decision, not a failure.
      if (result.applied && action === "joinGroup") reloginNeeded = true;
      if (result.applied && action === "leaveGroup") reloginNeeded = false;
      await refreshPrivileges();
    } catch (e) {
      grantError = errorText(e);
    } finally {
      granting = null;
    }
  }

  onMount(refreshPrivileges);

  /** The daemon's own view of what it was started with. */
  const daemonPrivileges = $derived(telemetry.systemInfo?.privileges ?? null);

  type Row = {
    id: string;
    ok: boolean;
    title: string;
    detail: string;
    action?: AdminAction;
    /** Takes back what `action` grants. Only offered for something that is
     *  granted - and, where it is a file, a file Pyren wrote. */
    revoke?: AdminAction;
    /** What revoking costs, shown on hover. */
    revokeHint?: string;
  };

  const rows = $derived.by<Row[]>(() => {
    const p = privileges;
    if (!p) return [];
    return [
      {
        id: "service",
        ok: p.serviceActive,
        title: t("admin.service"),
        detail: p.unitPath
          ? p.serviceActive
            ? t("admin.serviceRunning", { path: p.unitPath })
            : t("admin.serviceStopped", { path: p.unitPath })
          : p.daemonBinary
            ? t("admin.serviceInstallable", { binary: p.daemonBinary })
            : t("admin.serviceNoBinary"),
        // Installing the unit is what makes the daemon run as root, which
        // is why it is offered here and not only after the daemon is up.
        action: p.unitPath
          ? p.serviceActive
            ? undefined
            : "enableService"
          : p.daemonBinary
            ? "installService"
            : undefined,
        // Enabled-but-failed counts too: it would come back at boot. The
        // unit file stays, so granting it again is the enable above.
        revoke: p.serviceActive || p.serviceEnabled ? "disableService" : undefined,
        revokeHint: t("admin.revokeService"),
      },
      {
        id: "group",
        ok: p.sessionHasGroup,
        title: t("admin.group", { group: p.groupName }),
        detail: p.needsRelogin
          ? t("admin.groupNeedsRelogin")
          : p.leaveNeedsRelogin
            ? t("admin.groupLeftNeedsRelogin")
            : p.sessionHasGroup
              ? t("admin.groupOk")
              : t("admin.groupMissing", { group: p.groupName }),
        // Keyed on the database, not the session: both halves of the
        // log-out gap are one click from being undone.
        action: p.inGroupDatabase ? undefined : "joinGroup",
        revoke: p.inGroupDatabase ? "leaveGroup" : undefined,
        revokeHint: t("admin.revokeGroup", { group: p.groupName }),
      },
      {
        id: "socket",
        ok: p.socketReachable,
        title: t("admin.socket"),
        detail: p.socketReachable
          ? t("admin.socketOk", { path: p.socketPath })
          : p.socketDenied
            ? t("admin.socketDenied", { path: p.socketPath })
            : t("admin.socketUnreachable", { path: p.socketPath }),
      },
      {
        id: "perf",
        ok: daemonPrivileges?.perfEvents ?? false,
        title: t("admin.perfEvents"),
        detail: !daemonPrivileges
          ? t("admin.perfUnknown")
          : daemonPrivileges.perfEvents
            ? t("admin.perfOk")
            : daemonPrivileges.root
              ? t("admin.perfNoIntel")
              : t("admin.perfNotRoot"),
      },
      {
        // The one row here that is not about privilege at all, and it
        // earns its place for that reason: a missing kernel module fails
        // with the same "permission denied" as everything else on this
        // page, which sends people to the wrong fix entirely.
        id: "acpiCall",
        ok: p.acpiCallLoaded,
        title: t("admin.acpiCall"),
        detail: p.acpiCallLoaded
          ? t("admin.acpiCallOk")
          : p.acpiCallInstalled
            ? t("admin.acpiCallNotLoaded")
            : t("admin.acpiCallMissing"),
        // Only offered when there is a module to load. Where it is not
        // installed the fix is a package, which is not this button's job
        // and is named in the detail instead.
        action: !p.acpiCallLoaded && p.acpiCallInstalled ? "loadAcpiCall" : undefined,
        revoke: p.acpiCallLoaded || p.acpiCallAtBoot ? "unloadAcpiCall" : undefined,
        revokeHint: t("admin.revokeAcpiCall"),
      },
      {
        // The GPU offsets. Three outcomes rather than two, because "not
        // working" splits into a fix this button can apply and one it
        // cannot: on Wayland there is no NVIDIA X screen for Coolbits to
        // apply to, and writing the file would change nothing.
        id: "gpuOffsets",
        ok: p.nvmlOffsets || p.coolbitsSet,
        title: t("admin.gpuOffsets"),
        detail: p.nvmlOffsets
          ? t("admin.gpuOffsetsNvml")
          : p.coolbitsSet
            ? t("admin.gpuOffsetsCoolbitsSet")
            : p.coolbitsWouldHelp
              ? t("admin.gpuOffsetsNeedsCoolbits")
              : t("admin.gpuOffsetsWayland"),
        action: p.coolbitsWouldHelp ? "enableCoolbits" : undefined,
        // Only our own snippet: a Coolbits line somebody else wrote into
        // their xorg.conf is not this button's to delete.
        revoke: p.coolbitsOurs ? "disableCoolbits" : undefined,
        revokeHint: t("admin.revokeCoolbits"),
      },
      {
        id: "root",
        ok: daemonPrivileges?.root ?? false,
        title: t("admin.rootTitle"),
        // Three different reasons to be unprivileged, and telling someone
        // to "install the service" while a privileged service is already
        // running is worse than saying nothing.
        detail: !daemonPrivileges
          ? t("admin.rootUnknown")
          : daemonPrivileges.root
            ? t("admin.rootOk")
            : p.serviceActive
              ? t("admin.rootShadowed", { path: p.socketPath })
              : t("admin.rootNo"),
      },
    ];
  });

  const icons: Record<CheckStatus, string> = {
    pass: "check",
    fail: "close",
    warn: "warning",
    skip: "info",
  };
</script>

<div class="drivers">
  <h1 class="page-title">{t("diagnostics.title")}</h1>

  <div class="drivers-scroll">
  <!-- Privileges first: a fan check on a machine whose daemon cannot be
       reached only ever reports the same thing twice. -->
  {#if canInspect}
    <Panel title={t("admin.title")}>
      <p class="hint">{t("admin.intro")}</p>
      <hr class="sep" />

      {#if !privileges}
        <!-- Without this the panel vanished whenever the status call
             failed, taking the error message inside it along - the one
             state where the user most needs to be told something. -->
        <p class="notice">{statusError ?? t("common.loading")}</p>
      {/if}

      <ul class="checks">
        {#each rows as row (row.id)}
          <li class={row.ok ? "pass" : "warn"}>
            <Icon name={row.ok ? "check" : "warning"} size={15} />
            <div class="body">
              <span class="check-title">{row.title}</span>
              <span class="detail">{row.detail}</span>
            </div>
            <div class="actions">
              {#if row.action}
                <button
                  class="fix"
                  disabled={granting !== null || !privileges?.canElevate}
                  onclick={() => applyGrant(row.action!)}
                >
                  {granting === row.action ? t("admin.applying") : t("admin.fix")}
                </button>
              {/if}
              {#if row.revoke}
                <button
                  class="fix revoke"
                  title={row.revokeHint}
                  disabled={granting !== null || !privileges?.canElevate}
                  onclick={() => applyGrant(row.revoke!)}
                >
                  {granting === row.revoke ? t("admin.revoking") : t("admin.revoke")}
                </button>
              {/if}
            </div>
          </li>
        {/each}
      </ul>

      {#if reloginNeeded}
        <p class="notice warn">{t("admin.groupNeedsRelogin")}</p>
      {/if}
      {#if privileges && !privileges.canElevate}
        <p class="notice warn">{t("admin.noPolkit")}</p>
      {/if}
      {#if grantError}
        <p class="notice err">{grantError}</p>
      {/if}
    </Panel>
  {/if}

  <Panel title={t("diagnostics.checkPanelTitle")}>
    <div class="check">
      <div class="controls">
        <button class="run" onclick={run} disabled={running}>
          <Icon name="refresh" size={15} />
          {running ? t("diagnostics.running") : t("diagnostics.runCheck")}
        </button>

        <label class="writes">
          <Toggle
            checked={allowWrites}
            onchange={(v) => (allowWrites = v)}
            ariaLabel={t("diagnostics.allowWrites")}
          />
          <span>
            {t("diagnostics.allowWrites")}
            <InfoTip>{t("diagnostics.allowWritesHint")}</InfoTip>
          </span>
        </label>
      </div>

      {#if error}
        <details class="result warn" open>
          <summary>{t("diagnostics.resultDriver")}</summary>
          <p class="notice err">{error}</p>
        </details>
      {:else if diagnosis}
        <details class="result {diagnosis.verdict === 'fullControl' ? '' : 'warn'}" open>
          <summary>{t("diagnostics.resultDriver")}</summary>

          <!-- The point of the whole page: when control is missing, say what
               could fix it, rather than silently offering to install anything. -->
          {#if diagnosis.driverNotice}
            <p class="notice warn">{tm(diagnosis.driverNotice)}</p>
          {/if}
          {#if diagnosis.wroteToHardware}
            <p class="notice">{t("diagnostics.wroteToHardware")}</p>
          {/if}

          <ul class="checks">
            {#each diagnosis.checks as check (check.id)}
              <li class={check.status}>
                <Icon name={icons[check.status]} size={15} />
                <div class="body">
                  <span class="check-title">{tm(check.title)}</span>
                  <span class="detail">{tm(check.detail)}</span>
                  {#if check.remedy}
                    <span class="remedy">
                      <strong>{t("diagnostics.remedy")}:</strong>
                      {tm(check.remedy)}
                    </span>
                  {/if}
                </div>
              </li>
            {/each}
          </ul>
        </details>
      {:else if telemetry.demo}
        <p class="notice">{t("notices.daemonDownBody")}</p>
      {:else}
        <p class="notice">{t("diagnostics.neverRun")}</p>
      {/if}

      <p class="hint">{@html t("diagnostics.cliHint")}</p>
    </div>

    <hr class="sep" />
    <div class="check">
      <div class="controls">
        <button class="run" onclick={checkRgbKeyboard} disabled={rgbChecking}>
          <Icon name="refresh" size={15} />
          {rgbChecking ? t("diagnostics.checkingRgb") : t("diagnostics.checkRgb")}
        </button>
      </div>

      {#if rgbResult}
        <details class="result {rgbResult.ok ? '' : 'warn'}" open>
          <summary>{t("diagnostics.viewResult")}</summary>
          <p class="notice {rgbResult.ok ? '' : 'warn'}">{rgbResult.text}</p>
        </details>
      {/if}

      <p class="hint">{t("diagnostics.checkRgbHint")}</p>
    </div>
  </Panel>

  <!-- Last on the page on purpose: everything above answers "do I need
       this?", and on most machines the answer is no. -->
  <DriverWizard />
  </div>
</div>

<style>
  .drivers {
    flex: 1;
    min-height: 0;
    overflow: hidden;
    padding: 0 30px 32px;
    display: flex;
    flex-direction: column;
    max-width: 990px;
  }

  .drivers-scroll {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding-right: 6px;
    display: flex;
    flex-direction: column;
    gap: 14px;
  }

  .page-title {
    font-size: 24px;
    flex: 0 0 auto;
    margin: 0;
    padding: 24px 0 14px;
  }

  .actions {
    flex: 0 0 auto;
    align-self: center;
    margin-left: auto;
    display: flex;
    gap: 8px;
  }

  .fix {
    padding: 6px 16px;
    border: 1px solid var(--accent-2);
    border-radius: 2px;
    background: transparent;
    color: var(--text);
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }

  /* Quieter than a fix: it is the way back, not the thing the row asks for. */
  .fix.revoke {
    border-color: var(--line);
    color: var(--text-dim);
  }

  .fix.revoke:not(:disabled):hover {
    border-color: var(--danger);
    color: var(--danger);
  }

  .fix:disabled {
    opacity: 0.45;
  }

  .controls {
    display: flex;
    align-items: center;
    gap: 26px;
    flex-wrap: wrap;
  }

  .run {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    padding: 9px 20px;
    border: none;
    border-radius: 2px;
    background: var(--invert-bg);
    color: var(--invert-text);
    font-size: 12px;
    font-weight: 700;
    letter-spacing: 0.05em;
    text-transform: uppercase;
  }

  .run:disabled {
    opacity: 0.5;
  }

  .writes {
    display: flex;
    align-items: center;
    gap: 12px;
    font-size: 13px;
    color: var(--text-dim);
  }

  .check {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 10px;
  }

  .hint {
    margin: 0;
    color: var(--text-mute);
    font-size: 12px;
    line-height: 1.5;
  }

  .result {
    width: 100%;
  }

  .result summary {
    cursor: pointer;
    margin-bottom: 8px;
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.05em;
    text-transform: uppercase;
    color: var(--ok);
  }

  .result.warn summary {
    color: var(--warn);
  }

  .result .notice {
    margin: 8px 0 0;
  }

  .sep {
    margin: 16px 0;
    border: none;
    border-top: 1px solid var(--line-soft);
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

  .checks {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
  }

  .checks li {
    display: flex;
    gap: 12px;
    align-items: flex-start;
    padding: 10px 0;
    border-bottom: 1px solid var(--line-soft);
  }

  .checks li:last-child {
    border-bottom: none;
  }

  .checks li.pass {
    color: var(--ok);
  }
  .checks li.fail {
    color: var(--danger);
  }
  .checks li.warn {
    color: var(--warn);
  }
  .checks li.skip {
    color: var(--text-mute);
  }

  .body {
    display: flex;
    flex-direction: column;
    gap: 3px;
  }

  .check-title {
    color: var(--text);
    font-size: 14px;
  }

  .detail,
  .remedy {
    color: var(--text-dim);
    font-size: 12.5px;
    line-height: 1.45;
  }

  .remedy {
    color: var(--text-mute);
  }
</style>
