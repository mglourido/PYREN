import { settings } from "$lib/stores/settings.svelte";
import { notifications } from "$lib/stores/notifications.svelte";

/** Where the update check and the help links point. */
export const REPO_URL = "https://github.com/mglourido/PYREN";
export const ISSUES_URL = `${REPO_URL}/issues`;
export const LATEST_RELEASE_API =
  "https://api.github.com/repos/mglourido/PYREN/releases/latest";

export const APP_VERSION = __APP_VERSION__;

export type UpdateCheck =
  | { state: "idle" }
  | { state: "checking" }
  | { state: "current" }
  | { state: "available"; version: string; url: string }
  | { state: "error"; message: string };

/** Auto-checks run at most this often. */
const AUTO_CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000;

/** Compares dotted numeric versions; non-numeric suffixes are ignored. */
export function isNewer(candidate: string, current: string): boolean {
  const parse = (v: string) =>
    v.replace(/^v/, "").split(".").map((part) => parseInt(part, 10) || 0);
  const a = parse(candidate);
  const b = parse(current);
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    const diff = (a[i] ?? 0) - (b[i] ?? 0);
    if (diff !== 0) return diff > 0;
  }
  return false;
}

/**
 * Asks GitHub for the newest published release.
 *
 * Deliberately a plain fetch with no token and no retry: it runs only when
 * the user presses the button, and a failure (offline, rate limited) is
 * reported in the UI rather than retried in the background.
 */
export async function checkForUpdate(): Promise<UpdateCheck> {
  try {
    const response = await fetch(LATEST_RELEASE_API, {
      headers: { Accept: "application/vnd.github+json" },
    });
    if (!response.ok) return { state: "error", message: `HTTP ${response.status}` };

    const release = (await response.json()) as { tag_name?: string; html_url?: string };
    const tag = release.tag_name;
    if (!tag) return { state: "error", message: "no tag_name in response" };

    return isNewer(tag, APP_VERSION)
      ? { state: "available", version: tag.replace(/^v/, ""), url: release.html_url ?? REPO_URL }
      : { state: "current" };
  } catch (e) {
    return { state: "error", message: String(e) };
  }
}

/**
 * The manual "check for updates" button in Help. Always hits GitHub, never
 * pushes a notification - the result is shown right there on the page - but
 * still stamps `lastUpdateCheckAt` so it counts as this cycle's check and
 * postpones the next automatic one by another 6h.
 */
export async function checkForUpdateManually(): Promise<UpdateCheck> {
  const result = await checkForUpdate();
  settings.set("lastUpdateCheckAt", Date.now());
  return result;
}

/**
 * Runs once at startup. A no-op unless `autoCheckUpdates` is on and at
 * least 6h have passed since the last check (manual or automatic) - there
 * is no timer or interval, just a stamped timestamp read on entry.
 */
export async function maybeAutoCheckForUpdate(): Promise<void> {
  if (!settings.current.autoCheckUpdates) return;
  const last = settings.current.lastUpdateCheckAt;
  if (last !== null && Date.now() - last < AUTO_CHECK_INTERVAL_MS) return;

  const result = await checkForUpdate();
  settings.set("lastUpdateCheckAt", Date.now());
  if (result.state !== "available") return;

  if (settings.current.notifyUpdateOnce && result.version === settings.current.lastNotifiedUpdateVersion) {
    return;
  }
  settings.set("lastNotifiedUpdateVersion", result.version);
  notifications.notifyUpdateAvailable(result.version, result.url);
}
