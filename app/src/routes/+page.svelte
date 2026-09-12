<script lang="ts">
  /**
   * Home: the "gaming performance toolkit" board. The Windows original
   * splits into AI-controlled and user-controlled columns; here the left
   * column holds what the daemon decides on its own (the background
   * Eco/Balanced watcher) and the right two hold manual controls.
   */
  import InfoTip from "$lib/components/InfoTip.svelte";
  import Icon from "$lib/components/Icon.svelte";
  import Segmented from "$lib/components/Segmented.svelte";
  import Toggle from "$lib/components/Toggle.svelte";
  import { t } from "$lib/i18n/index.svelte";
  import { hardware, type PowerMode } from "$lib/stores/hardware.svelte";
  import { telemetry, tempColor } from "$lib/stores/telemetry.svelte";
  import { formatTemp } from "$lib/stores/settings.svelte";

  const modes: { id: PowerMode; icon: string }[] = [
    { id: "eco", icon: "leaf" },
    { id: "balanced", icon: "diamond" },
    { id: "performance", icon: "bars" },
    { id: "unlimited", icon: "boltbars" },
  ];

  /** The two modes each power source's supervisor may call home. */
  const preferable = (ids: PowerMode[]) =>
    ids.map((id) => ({ value: id, label: t(`performance.modes.${id}`) }));
</script>

<div class="home">
  <h1 class="title">{t("home.title")}</h1>

  <div class="columns">
    <!-- Left: what runs on its own, in the background. -->
    <section class="column">
      <header class="col-head">{t("home.systemControlled")}</header>

      <article class="card hero">
        <div class="hero-art" aria-hidden="true"></div>
        <h2 class="card-title"><Icon name="sparkles" size={18} /> {t("app.name")}</h2>
        <div class="switch-row">
          <Toggle
            checked={hardware.state.autoEco}
            onchange={(v) => hardware.setAutoSwitch(v, hardware.state.autoPerformance)}
            ariaLabel={t("home.autoEco")}
          />
          <span>{t("home.autoEco")}<InfoTip>{t("home.autoEcoHint")}</InfoTip></span>
        </div>
        <div class="switch-row">
          <Toggle
            checked={hardware.state.autoPerformance}
            onchange={(v) => hardware.setAutoSwitch(hardware.state.autoEco, v)}
            ariaLabel={t("home.autoPerformance")}
          />
          <span>{t("home.autoPerformance")}<InfoTip>{t("home.autoPerformanceHint")}</InfoTip></span>
        </div>
        {#if hardware.state.autoPerformance && hardware.power}
          <div class="prefer-row">
            <span>{t("home.preferOnMains")}</span>
            <Segmented
              variant="pill"
              options={preferable(["balanced", "performance"])}
              value={hardware.power.auto.preferredOnMains}
              onchange={(v) => hardware.setAutoPreference("mains", v as PowerMode)}
            />
          </div>
        {/if}
        <!-- A mode picked by hand outranks the preference until the cable
             next moves; without saying so, the supervisor would look like
             it had stopped listening to the switches above. Not shown for
             Unlimited, which the supervisor leaves alone entirely. -->
        {#if hardware.power?.auto.enabled && hardware.power.autoManualBaseline && hardware.power.autoManualBaseline !== "unlimited"}
          <p class="following">
            {t("home.followingManual", {
              mode: t(`performance.modes.${hardware.power.autoManualBaseline}`),
            })}
          </p>
        {/if}
        <!-- The third rule, and the only one about the machine rather than
             about the wall socket. Hidden where there is no sensor to
             read: a switch that can never fire is worse than none. -->
        {#if hardware.power?.thermal.available}
          <div class="switch-row">
            <Toggle
              checked={hardware.power.auto.backOffWhenHot}
              onchange={(v) => hardware.setThermalBackOff(v)}
              ariaLabel={t("home.thermalBackOff")}
            />
            <span>
              {t("home.thermalBackOff")}<InfoTip>{t("home.thermalBackOffHint")}</InfoTip>
            </span>
          </div>
          {#if hardware.power.thermal.hot}
            <p class="hot-now">{t("home.runningHot")}</p>
          {/if}
        {/if}
      </article>

      <article class="card">
        <h2 class="card-title"><Icon name="gauge" size={18} /> {t("vitals.temperature")}</h2>
        <div class="temps">
          <div>
            <span class="digital big" style="color:{tempColor(telemetry.cpuTempC)}">
              {formatTemp(telemetry.cpuTempC)}
            </span>
            <small>{t("vitals.cpuTemp")}</small>
          </div>
          <div>
            <span class="digital big" style="color:{tempColor(telemetry.gpuTempC)}">
              {formatTemp(telemetry.gpuTempC)}
            </span>
            <small>{t("vitals.gpuTemp")}</small>
          </div>
        </div>
        <a class="card-link" href="/system/vitals">{t("tabs.vitals")} →</a>
      </article>
    </section>

    <!-- Middle: power and temperature. -->
    <section class="column">
      <header class="col-head">{t("home.userControlled")}</header>

      <article class="card">
        <div class="art art-power" aria-hidden="true"></div>
        <h2 class="card-title banner-title">
          <Icon name="diamond" size={18} />
          {t("home.powerAndTemp")}
        </h2>

        <a class="feature" href="/system/performance">
          <Icon name="bars" size={17} />{t("home.performanceControl")}
        </a>
        <p class="desc">{t("home.performanceControlDesc")}</p>

        <div class="mode-strip">
          {#each modes as mode (mode.id)}
            <button
              class="mini"
              class:active={hardware.state.powerMode === mode.id}
              title={t(`performance.modes.${mode.id}`)}
              onclick={() => hardware.setPowerMode(mode.id)}
            >
              <Icon name={mode.icon} size={24} stroke={1.4} />
            </button>
          {/each}
        </div>

        <a class="feature" href="/system/performance">
          <Icon name="boltbars" size={17} />{t("home.unlimited")}
        </a>
        <p class="desc">{t("home.unlimitedDesc")}</p>
      </article>

      <article class="card">
        <a class="feature" href="/system/advanced">
          <Icon name="chip" size={17} />{t("home.advancedTuning")}
        </a>
        <p class="desc">{t("home.advancedTuningDesc")}</p>
      </article>

      <article class="card">
        <a class="feature" href="/system/cleaning">
          <Icon name="broom" size={17} />{t("home.fanCleaning")}
        </a>
        <p class="desc">{t("home.fanCleaningDesc")}</p>
      </article>

      <article class="card">
        <a class="feature" href="/system/graphics">
          <Icon name="monitor" size={17} />{t("home.graphicsSwitcher")}
        </a>
        <p class="desc">{t("home.graphicsSwitcherDesc")}</p>
      </article>
    </section>

    <!-- Right: the rest of the toolkit. -->
    <section class="column">
      <header class="col-head">{t("home.moreTools")}</header>

      <article class="card">
        <div class="art art-optimizer" aria-hidden="true"></div>
        <h2 class="card-title banner-title">
          <Icon name="gauge" size={18} />
          {t("home.optimizer")}
        </h2>

        <a class="feature" href="/system/lighting">
          <Icon name="bulb" size={17} />{t("tabs.lighting")}
        </a>
        <p class="desc">{t("home.lightingDesc")}</p>

        <a class="feature" href="/system/keys">
          <Icon name="keyboard" size={17} />{t("tabs.keys")}
        </a>
        <p class="desc">{t("home.keysDesc")}</p>
      </article>

      <article class="card">
        <a class="feature" href="/system/network">
          <Icon name="network" size={17} />{t("home.networkBooster")}
        </a>
        <p class="desc">{t("home.networkBoosterDesc")}</p>
      </article>

      <article class="card">
        <a class="feature" href="/drivers">
          <Icon name="download" size={17} />{t("drivers.title")}
        </a>
        <p class="desc">{t("home.driversDesc")}</p>
      </article>
    </section>
  </div>
</div>

<style>
  .home {
    flex: 1;
    overflow-y: auto;
    padding: 22px 28px 40px;
  }

  .title {
    text-align: center;
    font-size: 25px;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    margin-bottom: 20px;
  }

  .columns {
    display: grid;
    grid-template-columns: minmax(280px, 1fr) minmax(320px, 1.15fr) minmax(320px, 1.15fr);
    gap: 16px;
    align-items: start;
  }

  /* The three-column floor is ~1000px wide before the sidebar; a
     half-screen window is well under that, so it has to fold down
     instead of forcing a horizontal scrollbar on the whole dashboard. */
  @media (max-width: 1150px) {
    .columns {
      grid-template-columns: repeat(2, minmax(280px, 1fr));
    }
  }

  @media (max-width: 760px) {
    .home {
      padding: 18px 16px 32px;
    }

    .columns {
      grid-template-columns: 1fr;
    }
  }

  .col-head {
    text-align: center;
    padding: 7px;
    margin-bottom: 12px;
    background: var(--bg-card);
    border-radius: var(--radius-sm);
    font-size: 12px;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--text-dim);
  }

  .column {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }

  .card {
    background: var(--bg-panel);
    border: 1px solid var(--line-soft);
    border-radius: var(--radius);
    padding: 16px 18px 18px;
    overflow: hidden;
  }

  /* The Windows app fills these headers with promo artwork; abstract
     gradients keep the same visual rhythm without shipping HP assets. */
  .art,
  .hero-art {
    height: 90px;
    margin: -16px -18px 14px;
  }

  .hero-art {
    background:
      radial-gradient(120% 140% at 15% 0%, rgba(229, 23, 140, 0.55), transparent 60%),
      radial-gradient(120% 140% at 85% 100%, rgba(47, 143, 255, 0.45), transparent 60%),
      linear-gradient(120deg, #241026, #101820);
  }

  .art-power {
    background:
      radial-gradient(120% 140% at 15% 0%, rgba(255, 138, 0, 0.5), transparent 60%),
      radial-gradient(120% 140% at 85% 100%, rgba(242, 55, 75, 0.45), transparent 60%),
      linear-gradient(120deg, #2a1508, #101820);
  }

  .art-optimizer {
    background:
      radial-gradient(120% 140% at 20% 10%, rgba(33, 224, 101, 0.35), transparent 55%),
      radial-gradient(130% 130% at 80% 90%, rgba(123, 47, 247, 0.5), transparent 60%),
      linear-gradient(120deg, #10202a, #1b1030);
  }

  .card-title {
    display: flex;
    align-items: center;
    gap: 9px;
    font-size: 16px;
    letter-spacing: 0.05em;
    text-transform: uppercase;
    margin-bottom: 12px;
  }

  .banner-title {
    margin-top: -6px;
  }

  .lead {
    margin: 0 0 12px;
    color: var(--text-dim);
    font-size: 13px;
  }

  .feature {
    display: flex;
    align-items: center;
    gap: 9px;
    margin-top: 14px;
    color: var(--text);
    font-size: 15px;
    text-decoration: underline;
    text-underline-offset: 3px;
  }

  .feature:hover {
    color: var(--accent-3);
  }

  .desc {
    margin: 6px 0 0;
    color: var(--text-dim);
    font-size: 13px;
  }

  .mode-strip {
    display: flex;
    gap: 8px;
    margin-top: 14px;
  }

  .mini {
    display: grid;
    place-items: center;
    width: 72px;
    height: 62px;
    border: 1px solid transparent;
    border-radius: 2px;
    background: var(--bg-card);
    color: var(--text-mute);
  }

  .mini:hover {
    color: var(--text-dim);
  }

  .mini.active {
    border-color: var(--text);
    color: var(--text);
  }

  /* Only appears while the machine is actually over the threshold, which
     is why it is allowed to be this loud. */
  .prefer-row {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 8px 10px;
    margin: 6px 0 2px 50px;
    font-size: 12px;
    color: var(--text-mute);
  }

  .prefer-row :global(button) {
    padding: 5px 12px;
    font-size: 11px;
  }

  .following {
    margin: 8px 0 0;
    color: var(--text-mute);
    font-size: 12px;
  }

  .hot-now {
    margin: 8px 0 0;
    color: var(--warn, #e0a33e);
    font-size: 13px;
  }

  .switch-row {
    display: flex;
    align-items: center;
    gap: 12px;
    margin-top: 10px;
    font-size: 13px;
    color: var(--text-dim);
  }

  .switch-row :global(.tip) {
    margin-left: 2px;
  }

  .temps {
    display: flex;
    gap: 26px;
  }

  .temps div {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .big {
    font-size: 30px;
    line-height: 1;
  }

  .temps small {
    color: var(--text-mute);
    font-size: 12px;
  }

  .card-link {
    display: inline-block;
    margin-top: 14px;
    color: var(--text-dim);
    font-size: 13px;
    text-decoration: none;
  }

  .card-link:hover {
    color: var(--text);
  }
</style>
