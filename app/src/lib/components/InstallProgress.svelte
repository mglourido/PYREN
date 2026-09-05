<!--
  The install, while it is happening.

  Installing the driver is one IPC call that unloads a kernel module,
  compiles another and regenerates the initramfs, and it takes the better
  part of a minute. Before this the window simply sat there: the button
  said "installing…", nothing else moved, and the whole report appeared at
  the end. Two things were wrong with that. Nobody could tell a long step
  from a hung one - `dkms-build` alone is most of the wait - and the rest
  of the app stayed clickable, so a fan mode could be sent to a daemon
  whose driver was, at that instant, unloaded.

  So this covers the window, and it advances on the daemon's own
  `installer.progress` events rather than on a timer. The bar is one
  segment per step because that is what is actually known: the steps come
  from the plan, and the daemon says which one it is on. A percentage
  would be a number nobody measured.
-->
<script lang="ts">
  import Icon from "$lib/components/Icon.svelte";
  import { t, tm } from "$lib/i18n/index.svelte";
  import type { Msg, StepStatus } from "$lib/api/daemon";

  /** One step's state, as this panel knows it. */
  export type LiveStep = {
    id: string;
    description: Msg;
    /** `null` while it runs, and before it has started. */
    status: StepStatus | null;
    detail: Msg | null;
    started: boolean;
  };

  type Props = {
    open: boolean;
    /** What is being done, for the heading. */
    title: string;
    steps: LiveStep[];
    /** Index of the step running now, or `null` when none is. */
    current: number | null;
    /** Lines already written, newest last. */
    log: string[];
    /** Set once the whole run is over. */
    finished: boolean;
    succeeded: boolean;
    /** Shown instead of the steps when the run could not start at all. */
    error?: string | null;
    onclose: () => void;
  };

  let {
    open,
    title,
    steps,
    current,
    log,
    finished,
    succeeded,
    error = null,
    onclose,
  }: Props = $props();

  const done = $derived(steps.filter((s) => s.status !== null).length);

  /** The line under the bar: what is happening, in words. */
  const nowDoing = $derived.by(() => {
    if (error) return null;
    if (finished) return null;
    if (current === null) return null;
    return steps[current] ?? null;
  });

  // The log grows from the bottom, which is only useful if it follows.
  let logEl = $state<HTMLDivElement | null>(null);
  $effect(() => {
    log.length;
    if (logEl) logEl.scrollTop = logEl.scrollHeight;
  });

  /**
   * Keyboard focus must not escape a panel that is blocking the rest of
   * the window: tabbing to a button behind it would defeat the point of
   * covering it. Only matters once there is something to press.
   */
  let closeEl = $state<HTMLButtonElement | null>(null);
  $effect(() => {
    if (finished && closeEl) closeEl.focus();
  });
</script>

{#if open}
  <!-- The backdrop is the click blocker, and it deliberately has no
       dismiss-on-click: leaving halfway through does not stop a kernel
       module being replaced, so offering it would be a lie. -->
  <div class="backdrop" role="dialog" aria-modal="true" aria-label={title}>
    <div class="panel">
      <header>
        <h2>{title}</h2>
        {#if !finished && !error}
          <span class="count">{done} / {steps.length}</span>
        {/if}
      </header>

      {#if error}
        <p class="failed-note">{error}</p>
      {:else}
        <!-- One segment per step. Filled means done, striped means
             running, empty means not reached. -->
        <div class="chain" aria-hidden="true">
          {#each steps as step, i (step.id)}
            <span
              class="seg"
              class:done={step.status !== null}
              class:running={step.started && step.status === null}
              class:bad={step.status === "failed"}
              class:warn={step.status === "warned"}
              title={tm(step.description)}
              style="--i: {i}"
            ></span>
          {/each}
        </div>

        {#if nowDoing}
          <p class="doing">
            <Icon name="refresh" size={16} class="spin" />
            {tm(nowDoing.description)}
          </p>
        {/if}

        <div class="log" bind:this={logEl} role="log" aria-live="polite">
          {#each log as line, i (i)}
            <div class="line">{line}</div>
          {/each}
        </div>
      {/if}

      {#if finished || error}
        <footer>
          <p class="verdict" class:ok={succeeded && !error} class:bad={!succeeded || error}>
            <Icon name={succeeded && !error ? "check" : "warning"} size={17} />
            {succeeded && !error ? t("install.done") : t("install.failedNote")}
          </p>
          <button class="primary" bind:this={closeEl} onclick={onclose}>
            {t("install.close")}
          </button>
        </footer>
      {/if}
    </div>
  </div>
{/if}

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    z-index: 200;
    display: grid;
    place-items: center;
    padding: 24px;
    background: rgba(0, 0, 0, 0.72);
    backdrop-filter: blur(3px);
  }

  .panel {
    width: min(640px, 100%);
    max-height: 100%;
    display: flex;
    flex-direction: column;
    gap: 14px;
    padding: 22px;
    border: 1px solid var(--line);
    border-radius: var(--radius-lg);
    background: var(--bg-panel);
    box-shadow: var(--shadow);
  }

  header {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 12px;
  }
  h2 {
    margin: 0;
    font-size: 1.05rem;
    font-weight: 600;
  }
  .count {
    font-family: var(--font-digital), monospace;
    font-size: 0.85rem;
    color: var(--text-mute);
  }

  .chain {
    display: flex;
    gap: 3px;
  }
  .seg {
    flex: 1;
    height: 6px;
    border-radius: var(--radius-pill);
    background: var(--bg-inset);
    transition: background 160ms ease;
  }
  .seg.done {
    background: var(--gradient);
  }
  .seg.warn {
    background: var(--warn);
  }
  .seg.bad {
    background: var(--danger);
  }
  .seg.running {
    background: linear-gradient(90deg, var(--accent-1), var(--accent-3), var(--accent-1));
    background-size: 200% 100%;
    animation: slide 1.1s linear infinite;
  }
  @keyframes slide {
    to {
      background-position: -200% 0;
    }
  }

  .doing {
    display: flex;
    align-items: center;
    gap: 8px;
    margin: 0;
    font-size: 0.92rem;
    color: var(--text-dim);
  }

  .log {
    flex: 1;
    min-height: 132px;
    max-height: 280px;
    overflow-y: auto;
    padding: 10px 12px;
    border: 1px solid var(--line-soft);
    border-radius: var(--radius);
    background: var(--bg-inset);
    font-family: var(--font-digital), monospace;
    font-size: 0.78rem;
    line-height: 1.55;
    color: var(--text-dim);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .line + .line {
    margin-top: 2px;
  }

  footer {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
  }
  .verdict {
    display: flex;
    align-items: center;
    gap: 7px;
    margin: 0;
    font-size: 0.92rem;
  }
  .verdict.ok {
    color: var(--ok);
  }
  .verdict.bad {
    color: var(--danger);
  }
  .failed-note {
    margin: 0;
    color: var(--danger);
    font-size: 0.92rem;
  }

  button.primary {
    padding: 8px 18px;
    border: 0;
    border-radius: var(--radius);
    background: var(--gradient);
    color: #fff;
    font: inherit;
    font-weight: 600;
    cursor: pointer;
  }

  .doing :global(.spin) {
    animation: spin 1.1s linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .seg.running,
    .doing :global(.spin) {
      animation: none;
    }
  }
</style>
