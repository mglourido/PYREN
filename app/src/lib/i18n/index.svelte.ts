/**
 * Translation runtime.
 *
 * Keeps the three-tier fallback the project settled on earlier
 * (main language -> user fallback -> app default), but resolves it from
 * bundled JSON instead of reading files at runtime: the previous
 * `Bun.file()` approach can't work here, because this code executes inside
 * the Tauri webview where there is no Bun and no filesystem access.
 *
 * Adding a language is still "drop a <iso>.json next to en.json" - the glob
 * below picks it up at build time and it shows up in Settings by itself.
 */

const modules = import.meta.glob("./locales/*.json", {
  eager: true,
}) as Record<string, { default: Record<string, unknown> }>;

export const catalogs: Record<string, Record<string, unknown>> = Object.fromEntries(
  Object.entries(modules).map(([path, mod]) => [
    path.slice(path.lastIndexOf("/") + 1, -".json".length),
    mod.default,
  ]),
);

/** Language the app falls back to last; it is guaranteed to be complete. */
export const DEFAULT_LOCALE = "en";

/** Display names come from the platform, so a new locale needs no table. */
export function localeName(code: string): string {
  try {
    const name = new Intl.DisplayNames([code], { type: "language" }).of(code);
    if (name) return name[0].toUpperCase() + name.slice(1);
  } catch {
    /* Intl.DisplayNames missing or unknown tag - fall through */
  }
  return code;
}

export const availableLocales = Object.keys(catalogs).sort();

/** Best guess from the desktop environment, used before settings load. */
export function detectLocale(): string {
  for (const tag of navigator.languages ?? [navigator.language]) {
    const base = tag.toLowerCase().split("-")[0];
    if (base in catalogs) return base;
  }
  return DEFAULT_LOCALE;
}

function lookup(catalog: Record<string, unknown> | undefined, key: string): string | undefined {
  if (!catalog) return undefined;
  let node: unknown = catalog;
  for (const part of key.split(".")) {
    if (typeof node !== "object" || node === null) return undefined;
    node = (node as Record<string, unknown>)[part];
  }
  return typeof node === "string" ? node : undefined;
}

class I18n {
  main = $state(DEFAULT_LOCALE);
  fallback = $state(DEFAULT_LOCALE);

  setLocales(main: string, fallback: string = DEFAULT_LOCALE) {
    this.main = main in catalogs ? main : DEFAULT_LOCALE;
    this.fallback = fallback in catalogs ? fallback : DEFAULT_LOCALE;
    document.documentElement.lang = this.main;
  }

  /**
   * Resolve `key`, interpolating `{name}` placeholders from `params`.
   * A key missing from every catalog returns the key itself - visible in
   * the UI on purpose, so untranslated strings are easy to spot instead of
   * silently rendering as empty text.
   */
  t = (key: string, params?: Record<string, string | number>): string => {
    const raw =
      lookup(catalogs[this.main], key) ??
      lookup(catalogs[this.fallback], key) ??
      lookup(catalogs[DEFAULT_LOCALE], key) ??
      key;

    if (!params) return raw;
    return raw.replace(/\{(\w+)\}/g, (match, name) =>
      name in params ? String(params[name]) : match,
    );
  };
}

export const i18n = new I18n();
/** Shorthand so markup reads `{t("nav.home")}`. */
export const t = i18n.t;

/**
 * A translatable string as the daemon sends it (`pyren-core::Msg`):
 * `{ key, params?, text }`. `text` is the English wording, authoritative
 * when we have no catalog entry for `key`.
 */
export type Msg = {
  key: string;
  params?: Record<string, string | number | unknown>;
  text: string;
};

/**
 * Render a daemon [`Msg`] in the user's language: its catalog entry when
 * `key` resolves, otherwise the English `text` the daemon shipped. A joined
 * message (`key: ""`, `params.parts` a list) is rendered part by part and
 * re-joined, so each clause is still translated.
 */
export function tm(m: Msg | null | undefined): string {
  if (!m) return "";
  const parts = m.params?.parts;
  if (!m.key && Array.isArray(parts)) {
    return (parts as Msg[]).map(tm).join("; ");
  }
  if (!m.key) return m.text;
  const params = m.params as Record<string, string | number> | undefined;
  const rendered = t(m.key, params);
  // `t` echoes the key back when no catalog has it - fall back to the
  // wording the daemon already interpolated for us.
  return rendered === m.key ? m.text : rendered;
}
