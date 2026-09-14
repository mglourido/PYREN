/**
 * The debug-logging toggle ("Registros de depuración") and frontend error
 * capture, mirroring the shape of `$lib/api/admin.ts`.
 *
 * `status`/`setEnabled` forward to the daemon's `debug` module (see
 * docs/01-ipc-protocol.md). `installErrorCapture` is local to this
 * process: it writes straight to `~/.cache/pyren/depuration/frontend.jsonl`
 * through the Tauri shell and never touches the daemon.
 */

import { invoke } from "@tauri-apps/api/core";
import { inTauri } from "./daemon";

export type DebugLogStatus = {
  enabled: boolean;
  daemonDir: string;
  daemonDirWritable: boolean;
  userDir: string;
};

export const debugLog = {
  status: () => invoke<DebugLogStatus>("debug_get_status"),
  setEnabled: (enabled: boolean) => invoke<DebugLogStatus>("debug_set_enabled", { enabled }),
  /** False in a plain browser tab, where there is no shell to ask. */
  available: () => inTauri,

  /**
   * Attaches `window.onerror`/`unhandledrejection` handlers that forward
   * to `frontend.jsonl`. A no-op outside Tauri. Returns the function that
   * removes the handlers, same shape as `onDaemonEvent`.
   *
   * Every call is fire-and-forget: a failed write here must never itself
   * throw, or an error handler would become a new source of errors.
   */
  installErrorCapture(): () => void {
    if (!inTauri) return () => {};

    const onError = (event: ErrorEvent) => {
      void invoke("debug_log_frontend", {
        category: "error",
        entry: { message: event.message, source: event.filename, line: event.lineno },
      }).catch(() => {});
    };
    const onRejection = (event: PromiseRejectionEvent) => {
      void invoke("debug_log_frontend", {
        category: "error",
        entry: { message: String(event.reason) },
      }).catch(() => {});
    };

    window.addEventListener("error", onError);
    window.addEventListener("unhandledrejection", onRejection);
    return () => {
      window.removeEventListener("error", onError);
      window.removeEventListener("unhandledrejection", onRejection);
    };
  },

  /** A short, explicit breadcrumb - not every click. Call this from the
   *  handful of places worth remembering when reading a bug report. */
  action(name: string, detail?: Record<string, unknown>) {
    if (!inTauri) return;
    void invoke("debug_log_frontend", { category: "action", entry: { name, ...detail } }).catch(
      () => {},
    );
  },
};
