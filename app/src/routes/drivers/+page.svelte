<script lang="ts">
  /**
   * Driver / service installer.
   *
   * The privileged work (DKMS build, systemd unit, acpi_call) belongs in
   * the daemon-side installer flow; this page is its front end and shows
   * detected state plus what each component is for, so the user knows what
   * they are about to authorise.
   */
  import Icon from "$lib/components/Icon.svelte";
  import Panel from "$lib/components/Panel.svelte";
  import { t } from "$lib/i18n/index.svelte";
  import { telemetry } from "$lib/stores/telemetry.svelte";

  type Component = {
    id: string;
    title: string;
    desc: string;
    installed: boolean;
    required: boolean;
  };

  const components = $derived<Component[]>([
    {
      id: "hp-wmi",
      title: t("drivers.hpWmi"),
      desc: t("drivers.hpWmiDesc"),
      installed: telemetry.driverInstalled,
      required: true,
    },
    {
      id: "daemon",
      title: t("drivers.daemon"),
      desc: t("drivers.daemonDesc"),
      installed: !telemetry.demo,
      required: true,
    },
    {
      id: "acpi-call",
      title: t("drivers.acpiCall"),
      desc: t("drivers.acpiCallDesc"),
      installed: false,
      required: false,
    },
  ]);
</script>

<div class="drivers">
  <h1 class="page-title">{t("drivers.title")}</h1>

  {#each components as component (component.id)}
    <Panel>
      <div class="item">
        <div class="text">
          <h2>
            {component.title}
            {#if !component.required}<span class="opt">optional</span>{/if}
          </h2>
          <p>{component.desc}</p>
        </div>

        <div class="state" class:ok={component.installed}>
          <Icon name={component.installed ? "check" : "close"} size={16} />
          {component.installed ? t("drivers.installed") : t("drivers.notInstalled")}
        </div>

        <button class="action" disabled>
          {component.installed ? t("drivers.uninstall") : t("drivers.install")}
        </button>
      </div>
    </Panel>
  {/each}

  <p class="note"><Icon name="info" size={14} /> {t("drivers.needsRoot")}</p>
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

  .item {
    display: grid;
    grid-template-columns: 1fr auto auto;
    gap: 20px;
    align-items: center;
  }

  h2 {
    display: flex;
    align-items: center;
    gap: 10px;
    font-size: 15px;
  }

  .opt {
    padding: 1px 7px;
    border: 1px solid var(--line);
    border-radius: 2px;
    font-size: 10px;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--text-mute);
  }

  p {
    margin: 6px 0 0;
    color: var(--text-dim);
    font-size: 13px;
    line-height: 1.45;
    max-width: 560px;
  }

  .state {
    display: flex;
    align-items: center;
    gap: 7px;
    font-size: 13px;
    color: var(--text-mute);
    white-space: nowrap;
  }

  .state.ok {
    color: var(--ok);
  }

  .action {
    padding: 8px 20px;
    border: 1px solid var(--line);
    border-radius: 2px;
    background: transparent;
    color: var(--text);
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }

  .action:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .note {
    display: flex;
    align-items: center;
    gap: 8px;
    color: var(--text-mute);
    font-size: 12px;
  }
</style>
