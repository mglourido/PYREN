<script lang="ts">
  /**
   * Every icon the UI uses, as inline SVG paths.
   *
   * Inline rather than an icon package so the bundle has no runtime icon
   * dependency and the OMEN-specific glyphs (the eco leaf, the balanced
   * diamond, the "unlimited" bolt-bars) can be drawn to match the
   * reference app instead of approximated with a generic set.
   */
  type Props = {
    name: string;
    size?: number;
    stroke?: number;
    class?: string;
  };
  let { name, size = 20, stroke = 1.6, class: klass = "" }: Props = $props();

  const paths: Record<string, string> = {
    // sidebar / chrome
    home: "M3 10.5 12 3l9 7.5V21h-6v-6H9v6H3z",
    gauge: "M12 21a9 9 0 1 1 9-9 M12 21a9 9 0 0 1-9-9 M12 12l4-4",
    sparkles: "M12 3l1.8 4.7L18.5 9.5l-4.7 1.8L12 16l-1.8-4.7L5.5 9.5l4.7-1.8z M18 15l.9 2.3 2.3.9-2.3.9L18 21l-.9-2.3-2.3-.9 2.3-.9z",
    layers: "M12 3 3 7.5 12 12l9-4.5z M3 12l9 4.5L21 12 M3 16.5 12 21l9-4.5",
    laptop: "M4 5h16v10H4z M2 19h20 M9 19h6",
    settings:
      "M12 15.5a3.5 3.5 0 1 0 0-7 3.5 3.5 0 0 0 0 7z M19.4 13.5a1.7 1.7 0 0 0 .3 1.9l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-2.9 1.2v.2a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-3-1.2l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0-1.2-2.9h-.2a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.2-3l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 2.9-1.2v-.2a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 3 1.2l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0 1.2 2.9h.2a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.6 1z",
    help: "M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18z M9.6 9.3a2.5 2.5 0 0 1 4.9.7c0 1.7-2.5 2.5-2.5 2.5 M12 17h.01",
    download: "M12 3v12 M7 11l5 5 5-5 M4 20h16",
    chip: "M7 7h10v10H7z M4 9h3 M4 15h3 M17 9h3 M17 15h3 M9 4v3 M15 4v3 M9 17v3 M15 17v3",
    network: "M12 20h.01 M8.5 16.5a5 5 0 0 1 7 0 M5 13a10 10 0 0 1 14 0 M2 9.5a15 15 0 0 1 20 0",
    keyboard: "M3 6h18v12H3z M7 10h.01 M11 10h.01 M15 10h.01 M8 14h8",
    bulb: "M9 18h6 M10 21h4 M12 3a6 6 0 0 1 4 10.5V16H8v-2.5A6 6 0 0 1 12 3z",
    monitor: "M3 5h18v11H3z M8 20h8 M12 16v4",
    battery: "M3 8h14v8H3z M20 11v2",
    rocket: "M12 3c3.5 2 5.5 5.5 5.5 9.5L12 18l-5.5-5.5C6.5 8.5 8.5 5 12 3z M9 19l-2 2 M15 19l2 2 M12 11h.01",
    broom: "M14 3 9 8 M6 21l-2-2 6-9 5 5-9 6z M13 6l5 5",
    // power modes
    leaf: "M5 19c0-8 5-13 14-13 0 9-5 13-11 13H5z M5 19c2-4 5-7 9-9",
    diamond: "M12 3 21 12l-9 9-9-9z M12 8l4 4-4 4-4-4z",
    bars: "M5 20V12 M12 20V5 M19 20v-9",
    boltbars: "M4 20v-7 M10 20V9 M16 20v-5 M20 3l-4 7h4l-4 6",
    // controls
    chevronLeft: "M15 5l-7 7 7 7",
    chevronRight: "M9 5l7 7-7 7",
    chevronDown: "M6 9l6 6 6-6",
    chevronUp: "M6 15l6-6 6 6",
    check: "M5 12.5 9.5 17 19 7.5",
    close: "M6 6l12 12 M18 6 6 18",
    minimize: "M5 12h14",
    maximize: "M5 5h14v14H5z",
    warning: "M12 3 22 20H2z M12 10v4 M12 17h.01",
    info: "M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18z M12 11v6 M12 7.5h.01",
    search: "M11 18a7 7 0 1 0 0-14 7 7 0 0 0 0 14z M16.5 16.5 21 21",
    trash: "M4 7h16 M9 7V4h6v3 M6 7l1 13h10l1-13",
    refresh: "M20 12a8 8 0 1 1-2.3-5.6 M20 4v5h-5",
    external: "M14 4h6v6 M20 4l-9 9 M18 14v6H4V6h6",
    plug: "M9 3v6 M15 3v6 M6 9h12v3a6 6 0 0 1-12 0z M12 18v3",
  };

  const path = $derived(paths[name] ?? paths.info);
</script>

<svg
  class={klass}
  width={size}
  height={size}
  viewBox="0 0 24 24"
  fill="none"
  stroke="currentColor"
  stroke-width={stroke}
  stroke-linecap="round"
  stroke-linejoin="round"
  aria-hidden="true"
>
  <path d={path} />
</svg>
