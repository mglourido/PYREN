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
  import Icon from "$lib/components/Icon.svelte";
  import InfoTip from "$lib/components/InfoTip.svelte";
  import Panel from "$lib/components/Panel.svelte";
  import Toggle from "$lib/components/Toggle.svelte";
  import { daemon, type FanDiagnosis, type CheckStatus } from "$lib/api/daemon";
  import { t } from "$lib/i18n/index.svelte";
  import { telemetry } from "$lib/stores/telemetry.svelte";

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
      error = String(e);
      diagnosis = null;
    } finally {
      running = false;
    }
  }

  const icons: Record<CheckStatus, string> = {
    pass: "check",
    fail: "close",
    warn: "warning",
    skip: "info",
  };
</script>

<div class="drivers">
  <h1 class="page-title">{t("diagnostics.title")}</h1>

  <Panel>
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

    <p class="hint">{@html t("diagnostics.cliHint")}</p>

    {#if error}
      <p class="notice err">{error}</p>
    {:else if telemetry.demo && !diagnosis}
      <p class="notice">{t("notices.daemonDownBody")}</p>
    {:else if !diagnosis}
      <p class="notice">{t("diagnostics.neverRun")}</p>
    {/if}
  </Panel>

  {#if diagnosis}
    <Panel>
      <div class="verdict {diagnosis.verdict}">
        <Icon
          name={diagnosis.verdict === "fullControl" ? "check" : "warning"}
          size={22}
        />
        <div>
          <strong>{t(`diagnostics.verdict.${diagnosis.verdict}`)}</strong>
          <p>{diagnosis.summary}</p>
        </div>
      </div>

      <!-- The point of the whole page: when control is missing, say what
           could fix it, rather than silently offering to install anything. -->
      {#if diagnosis.driverNotice}
        <p class="notice warn">{diagnosis.driverNotice}</p>
      {/if}
      {#if diagnosis.wroteToHardware}
        <p class="notice">{t("diagnostics.wroteToHardware")}</p>
      {/if}
    </Panel>

    <Panel title={t("diagnostics.checks")}>
      <ul class="checks">
        {#each diagnosis.checks as check (check.id)}
          <li class={check.status}>
            <Icon name={icons[check.status]} size={15} />
            <div class="body">
              <span class="check-title">{check.title}</span>
              <span class="detail">{check.detail}</span>
              {#if check.remedy}
                <span class="remedy">
                  <strong>{t("diagnostics.remedy")}:</strong>
                  {check.remedy}
                </span>
              {/if}
            </div>
          </li>
        {/each}
      </ul>
    </Panel>
  {/if}
</div>

<style>
  .drivers {
    flex: 1;
    overflow-y: auto;
    padding: 24px 30px 44px;
    display: flex;
    flex-direction: column;
    gap: 14px;
    max-width: 900px;
  }

  .page-title {
    font-size: 24px;
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
    background: #f2f2f4;
    color: #17171a;
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

  .hint {
    margin: 14px 0 0;
    color: var(--text-mute);
    font-size: 12px;
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
    gap: 14px;
    align-items: flex-start;
  }

  .verdict strong {
    font-size: 16px;
  }

  .verdict p {
    margin: 5px 0 0;
    color: var(--text-dim);
    font-size: 13px;
    line-height: 1.5;
  }

  .verdict.fullControl {
    color: var(--ok);
  }

  .verdict.monitoringOnly {
    color: var(--warn);
  }

  .verdict.unsupported {
    color: var(--text-mute);
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
