<script lang="ts">
  /**
   * Help / about: what this is, how to get involved, the legal position,
   * the detected hardware (so a bug report can include it) and the update
   * check.
   */
  import Icon from "$lib/components/Icon.svelte";
  import Panel from "$lib/components/Panel.svelte";
  import { t } from "$lib/i18n/index.svelte";
  import { telemetry } from "$lib/stores/telemetry.svelte";
  import {
    APP_VERSION,
    ISSUES_URL,
    REPO_URL,
    checkForUpdate,
    type UpdateCheck,
  } from "$lib/version";

  let update = $state<UpdateCheck>({ state: "idle" });

  async function check() {
    update = { state: "checking" };
    update = await checkForUpdate();
  }

  const info = $derived(telemetry.systemInfo);
  const unknown = $derived(t("system.unknown"));
</script>

<div class="help">
  <h1 class="page-title">{t("help.title")}</h1>

  <Panel title={t("help.about")}>
    <p>{t("help.aboutBody")}</p>

    <div class="version">
      <span>{t("help.version")} <b>{APP_VERSION}</b></span>
      <button class="ghost" onclick={check} disabled={update.state === "checking"}>
        <Icon name="refresh" size={15} />
        {update.state === "checking" ? t("help.checking") : t("help.checkUpdates")}
      </button>
    </div>

    {#if update.state === "current"}
      <p class="ok">{t("help.upToDate")}</p>
    {:else if update.state === "available"}
      <p class="ok">
        {t("help.updateAvailable", { version: update.version })}
        <a href={update.url} target="_blank" rel="noreferrer">{t("common.learnMore")}</a>
      </p>
    {:else if update.state === "error"}
      <p class="err">{update.message}</p>
    {/if}
  </Panel>

  <Panel title={t("help.system")}>
    <dl class="specs">
      <dt>{t("system.vendor")}</dt>
      <dd>{info?.vendor ?? unknown}</dd>
      <dt>{t("system.model")}</dt>
      <dd>{info?.model ?? unknown}</dd>
      <dt>{t("system.board")}</dt>
      <dd>{info?.boardName ?? unknown}{info?.boardVendor ? ` (${info.boardVendor})` : ""}</dd>
      <dt>{t("system.formFactor")}</dt>
      <dd>{info ? t(`system.${info.formFactor}`) : unknown}</dd>
      <dt>{t("system.biosVersion")}</dt>
      <dd>{info?.biosVersion ?? unknown}{info?.biosDate ? ` — ${info.biosDate}` : ""}</dd>
      <dt>{t("system.kernel")}</dt>
      <dd>{info?.kernel ?? unknown}</dd>
      <dt>{t("system.cpu")}</dt>
      <dd>{info?.cpu ?? unknown}{info?.cpuCores ? ` (${info.cpuCores} threads)` : ""}</dd>
      <dt>{t("system.gpu")}</dt>
      <dd>{info?.gpus?.join(", ") || unknown}</dd>
    </dl>

    <p
      class="compat"
      class:ok={info?.compatibility === "supported"}
      class:warn={info?.compatibility === "untested"}
      class:err={info?.compatibility === "unsupported"}
    >
      <Icon name={info?.compatibility === "supported" ? "check" : "warning"} size={15} />
      <span>
        {#if info?.compatibility === "supported"}{t("system.compatible")}
        {:else if info?.compatibility === "untested"}{t("system.untested")}
        {:else}{t("system.incompatible")}{/if}
      </span>
      {#if info?.reason}<span class="reason">— {info.reason}</span>{/if}
    </p>
  </Panel>

  <Panel title={t("help.contribute")}>
    <ul class="links">
      <li>
        <a href={REPO_URL} target="_blank" rel="noreferrer">
          <Icon name="external" size={15} />{t("help.repository")}
        </a>
      </li>
      <li>
        <a href={ISSUES_URL} target="_blank" rel="noreferrer">
          <Icon name="external" size={15} />{t("help.reportIssue")}
        </a>
      </li>
    </ul>

    <h3>{t("help.translations")}</h3>
    <p>{@html t("help.translationsBody")}</p>
  </Panel>

  <Panel title={t("help.legal")}>
    <p>{t("help.legalBody")}</p>
    <p class="muted">{t("help.license")}: MIT</p>
  </Panel>
</div>

<style>
  .help {
    flex: 1;
    overflow-y: auto;
    padding: 24px 30px 44px;
    display: flex;
    flex-direction: column;
    gap: 16px;
    max-width: 860px;
  }

  .page-title {
    font-size: 24px;
  }

  p {
    margin: 0 0 12px;
    color: var(--text-dim);
    font-size: 14px;
    line-height: 1.55;
    user-select: text;
  }

  p:last-child {
    margin-bottom: 0;
  }

  .version {
    display: flex;
    align-items: center;
    gap: 18px;
    padding-top: 6px;
    font-size: 14px;
  }

  .ghost {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    padding: 7px 16px;
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--text);
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }

  .ghost:disabled {
    opacity: 0.5;
  }

  .ok {
    color: var(--ok);
    margin-top: 12px;
  }

  .err {
    color: var(--danger);
    margin-top: 12px;
  }

  .specs {
    display: grid;
    grid-template-columns: minmax(140px, auto) 1fr;
    gap: 6px 24px;
    margin: 0;
    font-size: 14px;
  }

  .specs dt {
    color: var(--text-mute);
  }

  .specs dd {
    margin: 0;
    user-select: text;
  }

  .compat {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 16px;
  }

  .warn {
    color: var(--warn);
  }

  .reason {
    color: var(--text-mute);
    font-size: 13px;
  }

  .links {
    list-style: none;
    margin: 0 0 18px;
    padding: 0;
    display: flex;
    gap: 22px;
  }

  .links a {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    font-size: 14px;
    color: var(--text);
  }

  .links a:hover {
    color: var(--accent-3);
  }

  h3 {
    font-size: 14px;
    margin-bottom: 8px;
  }
</style>
