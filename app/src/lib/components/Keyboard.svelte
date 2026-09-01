<script lang="ts">
  /**
   * Clickable ISO keyboard used by the key-mapping page.
   *
   * Layout is data, not markup, so a second layout (ANSI, or a
   * tenkeyless OMEN chassis) is a new array rather than a new component.
   * `id` is the value handed to the caller; `label` is what's printed on
   * the cap and can hold two lines separated by "\n" like the real caps.
   */
  export type Key = { id: string; label: string; w?: number; sub?: string };

  type Props = {
    selected?: string | null;
    mapped?: Record<string, string>;
    onselect: (id: string) => void;
  };
  let { selected = null, mapped = {}, onselect }: Props = $props();

  const k = (id: string, label = id, w = 1, sub?: string): Key => ({ id, label, w, sub });

  /** [main cluster, navigation cluster, numpad cluster] per row. */
  const rows: Key[][][] = [
    [
      [k("Escape", "ESC", 1.3)],
      [],
      [],
    ],
    [
      [
        k("Backquote", "º", 1, "ª\\"),
        k("Digit1", "1"), k("Digit2", "2"), k("Digit3", "3"), k("Digit4", "4"),
        k("Digit5", "5"), k("Digit6", "6"), k("Digit7", "7"), k("Digit8", "8"),
        k("Digit9", "9"), k("Digit0", "0"),
        k("Minus", "'", 1, "?"), k("Equal", "¡", 1, "¿"),
        k("Backspace", "⟵", 2),
      ],
      [k("Insert", "INSERT"), k("Home", "HOME"), k("PageUp", "PG UP")],
      [k("NumLock", ""), k("NumpadDivide", "/"), k("NumpadMultiply", "*"), k("NumpadSubtract", "-")],
    ],
    [
      [
        k("Tab", "TAB", 1.5),
        k("KeyQ", "Q"), k("KeyW", "W"), k("KeyE", "E"), k("KeyR", "R"), k("KeyT", "T"),
        k("KeyY", "Y"), k("KeyU", "U"), k("KeyI", "I"), k("KeyO", "O"), k("KeyP", "P"),
        k("BracketLeft", "`", 1, "[·"), k("BracketRight", "+", 1, "]·"),
        k("Backslash", "ç", 1.5, "}"),
      ],
      [k("Delete", "DEL"), k("End", "END"), k("PageDown", "PG DN")],
      [k("Numpad7", "7"), k("Numpad8", "8"), k("Numpad9", "9"), k("NumpadAdd", "+")],
    ],
    [
      [
        k("CapsLock", "CAPS", 1.8),
        k("KeyA", "A"), k("KeyS", "S"), k("KeyD", "D"), k("KeyF", "F"), k("KeyG", "G"),
        k("KeyH", "H"), k("KeyJ", "J"), k("KeyK", "K"), k("KeyL", "L"),
        k("Semicolon", "Ñ"), k("Quote", "´", 1, "{·"),
        k("Enter", "ENTER", 2.2),
      ],
      [],
      [k("Numpad4", "4"), k("Numpad5", "5"), k("Numpad6", "6")],
    ],
    [
      [
        k("ShiftLeft", "SHIFT", 2.4),
        k("KeyZ", "Z"), k("KeyX", "X"), k("KeyC", "C"), k("KeyV", "V"), k("KeyB", "B"),
        k("KeyN", "N"), k("KeyM", "M"),
        k("Comma", ",", 1, ";"), k("Period", ".", 1, ":"), k("Slash", "-", 1, "_"),
        k("ShiftRight", "SHIFT", 2.4),
      ],
      [k("ArrowUp", "▲")],
      [k("Numpad1", "1"), k("Numpad2", "2"), k("Numpad3", "3"), k("NumpadEnter", "ENTER")],
    ],
    [
      [
        k("ControlLeft", "CTRL", 1.5), k("MetaLeft", "❖"), k("AltLeft", "ALT", 1.3),
        k("Space", "", 7),
        k("AltRight", "ALT", 1.3), k("Fn", "FN"), k("ContextMenu", "▤"),
        k("IntlBackslash", ">", 1, "<"),
      ],
      [k("ArrowLeft", "◀"), k("ArrowDown", "▼"), k("ArrowRight", "▶")],
      [k("Numpad0", "0", 2), k("NumpadDecimal", ".")],
    ],
  ];

  /** F-keys sit in their own spaced groups above row 0. */
  const fnGroups = [
    [k("F1", "F1"), k("F2", "F2"), k("F3", "F3")],
    [k("F4", "F4"), k("F5", "F5"), k("F6", "F6")],
    [k("F7", "F7"), k("F8", "F8"), k("F9", "F9")],
    [k("F10", "F10"), k("F11", "F11"), k("F12", "F12")],
  ];
  const sysKeys = [k("PrintScreen", "PRT SC"), k("ScrollLock", "SCROLL\nLOCK"), k("Pause", "PAUSE\nBREAK")];
</script>

{#snippet cap(key: Key)}
  <button
    class="key"
    class:selected={selected === key.id}
    class:mapped={key.id in mapped}
    style="--w:{key.w ?? 1}"
    title={mapped[key.id] ? `${key.label} → ${mapped[key.id]}` : key.label}
    onclick={() => onselect(key.id)}
  >
    <span class="cap-label">{key.label}</span>
    {#if key.sub}<span class="cap-sub">{key.sub}</span>{/if}
  </button>
{/snippet}

<div class="board">
  <div class="row fnrow">
    <div class="cluster">{@render cap(k("Escape", "ESC", 1.3))}</div>
    {#each fnGroups as group, i (i)}
      <div class="cluster">{#each group as key (key.id)}{@render cap(key)}{/each}</div>
    {/each}
    <div class="cluster sys">{#each sysKeys as key (key.id)}{@render cap(key)}{/each}</div>
  </div>

  {#each rows.slice(1) as [main, nav, pad], rowIndex (rowIndex)}
    <div class="row">
      <div class="cluster main">{#each main as key (key.id)}{@render cap(key)}{/each}</div>
      <div class="cluster nav">{#each nav as key (key.id)}{@render cap(key)}{/each}</div>
      <div class="cluster pad">{#each pad as key (key.id)}{@render cap(key)}{/each}</div>
    </div>
  {/each}
</div>

<style>
  .board {
    display: inline-flex;
    flex-direction: column;
    gap: 6px;
    padding: 16px;
    background: linear-gradient(180deg, #17171a, #0e0e10);
    border: 1px solid var(--line);
    border-radius: var(--radius);
  }

  .row {
    display: flex;
    gap: 14px;
    align-items: flex-start;
  }

  .cluster {
    display: flex;
    gap: 4px;
  }

  .cluster.main {
    /* Keeps the alphanumeric block aligned across rows even though each row
       has a different key count. */
    width: 640px;
  }

  .cluster.nav {
    width: 154px;
  }

  .fnrow {
    margin-bottom: 6px;
  }

  .key {
    position: relative;
    flex: 0 0 auto;
    width: calc(var(--w) * 40px);
    height: 38px;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 1px;
    padding: 2px;
    border: 1px solid #2c2c31;
    border-radius: 4px;
    background: linear-gradient(180deg, #232327, #171719);
    color: var(--text-dim);
    font-size: 11px;
    white-space: pre-line;
    line-height: 1.05;
    text-align: center;
  }

  .key:hover {
    background: #303036;
    color: var(--text);
  }

  .key.selected {
    background: #1a4a8a;
    border-color: #2f8fff;
    color: #fff;
  }

  /* A remapped key is flagged with a small gradient corner marker. */
  .key.mapped::after {
    content: "";
    position: absolute;
    top: 3px;
    right: 3px;
    width: 5px;
    height: 5px;
    border-radius: 50%;
    background: var(--accent-3);
  }

  .cap-sub {
    font-size: 9px;
    color: var(--text-mute);
  }
</style>
