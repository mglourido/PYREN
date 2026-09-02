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
