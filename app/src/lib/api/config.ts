/**
 * Per-user settings on disk (`~/.config/pyren/<namespace>.json`).
 *
 * The files are written by the Tauri shell through `pyren-config`, the
 * same crate the daemon uses, so app settings get the same guarantees as
 * daemon settings: atomic writes, a corrupt file preserved rather than
 * silently replaced, and a version stamp.
 */

import { invoke } from "@tauri-apps/api/core";
import { inTauri } from "./daemon";

/** Namespaces must match `APP_CONFIG_NAMESPACES` in src-tauri/src/lib.rs. */
export type ConfigNamespace = "app" | "ui";

export type ConfigOutcome =
  | { status: "loaded" }
  | { status: "missing" }
  /** The file couldn't be parsed; `backup` is where it was kept. */
  | { status: "recovered"; reason: string; backup: string | null }
  /** Written by a newer build; left alone, defaults used for this run. */
  | { status: "tooNew"; found: number };

export type LoadedConfig = {
  values: Record<string, unknown>;
  path: string;
  outcome: ConfigOutcome;
};

export const appConfig = {
  load: (namespace: ConfigNamespace) =>
    invoke<LoadedConfig>("app_config_load", { namespace }),
  save: (namespace: ConfigNamespace, values: Record<string, unknown>) =>
    invoke<string>("app_config_save", { namespace, values }),
  available: () => inTauri,
};
