/**
 * Thin wrapper around the Tauri commands that proxy to omen-hub-daemon
 * (see docs/01-ipc-protocol.md). Everything the UI needs from the outside
 * world goes through here, so the rest of the frontend never imports
 * `@tauri-apps/api` directly and stays runnable in a plain browser tab
 * (`bun run dev`) for UI work.
 */

import { invoke } from "@tauri-apps/api/core";

/** False when the page is served by Vite in a normal browser, not Tauri. */
export const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

export type FanStatus = {
  driverInstalled: boolean;
  cpuTempC: number | null;
  fanRpm: number;
  isReverse: boolean;
};

export type ModuleCapability = { id: string; supported: boolean };

/** Hardware identity used for the compatibility check on startup. */
export type SystemInfo = {
  vendor: string | null;
  model: string | null;
  boardName: string | null;
  biosVersion: string | null;
  kernel: string | null;
  cpu: string | null;
  gpus: string[];
  supported: boolean;
};

export class DaemonUnavailable extends Error {}

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (!inTauri) throw new DaemonUnavailable("not running inside Tauri");
  try {
    return await invoke<T>(command, args);
  } catch (e) {
    throw new DaemonUnavailable(String(e));
  }
}

export const daemon = {
  capabilities: () => call<ModuleCapability[]>("core_capabilities"),
  systemInfo: () => call<SystemInfo>("core_system_info"),
  fanStatus: () => call<FanStatus>("fan_get_status"),
  /** Not implemented daemon-side yet; the UI already calls it. */
  setFanMode: (mode: "auto" | "manual" | "max", pwm?: number) =>
    call<null>("fan_set_mode", { mode, pwm }),
  setPowerMode: (mode: string) => call<null>("power_set_mode", { mode }),
};
