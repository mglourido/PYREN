/**
 * The app's notification history and its unread count.
 *
 * Only one thing raises a notification so far: the daemon's fan stall
 * watch, which nudges the fan floor up when the fans keep giving out at it
 * and publishes `fan.floorRaised`. Two paths carry the same events and are
 * reconciled by a content-derived id:
 *
 * - the live event bus (`onDaemonEvent`), which only fires inside Tauri;
 * - `fan.getStatus().floorNotices`, which the daemon persists so a window
 *   that was closed when it happened still catches up on the next poll.
 *
 * Read/unread state lives in `localStorage` - a per-machine convenience
 * that never needs to reach the daemon or another device, and losing it
 * only means a badge shows again.
 */

import { daemon, onDaemonEvent, type DaemonEvent, type FanStatus } from "$lib/api/daemon";
import { t } from "$lib/i18n/index.svelte";

/** How many notifications are kept. Older ones fall off the end. */
const MAX = 50;

const READ_KEY = "pyren.notifications.read";

export type NotificationKind = "info" | "warning";

/** One notification as the UI renders it. Title and body are resolved from
 *  the catalog on read, so they follow a language change. */
export type AppNotification = {
  id: string;
  topic: string;
  /** Unix seconds. */
  at: number;
  kind: NotificationKind;
  icon: string;
  title: string;
  body: string;
  /** A hint the panel turns into a shortcut, when the notification calls
   *  for a follow-up the user has to do. */
  action?: "recalibrate";
  read: boolean;
};

/** What is actually stored: the raw event, so it can be re-described in a
 *  different language later. */
type StoredEvent = {
  id: string;
  topic: string;
  at: number;
  data: Record<string, unknown>;
};

/** Turns a raw event into the parts the UI shows. One entry per topic;
 *  adding a notification kind is adding a row here. */
const DESCRIPTORS: Record<
  string,
  (data: Record<string, unknown>) => Omit<AppNotification, "id" | "topic" | "at" | "read">
> = {
  "fan.floorRaised": (data) => {
    const from = String(data.fromRpm ?? data.raisedFromRpm ?? "?");
    const to = String(data.toRpm ?? data.raisedToRpm ?? "?");
    const capped = Boolean(data.reachedDriverFloor);
    return {
      kind: capped ? "warning" : "info",
      icon: "fan",
      title: t("notifications.floorRaised.title"),
      body: capped
        ? t("notifications.floorRaised.bodyCapped", { from, to })
        : t("notifications.floorRaised.body", { from, to }),
      action: capped ? "recalibrate" : undefined,
    };
  },
  "app.updateAvailable": (data) => ({
    kind: "info",
    icon: "refresh",
    title: t("notifications.updateAvailable.title"),
    body: t("notifications.updateAvailable.body", { version: String(data.version ?? "?") }),
  }),
};

/** A stable id, so the live event and its persisted twin collapse into
 *  one. Each fan-floor raise moves both rpm figures, and a raise that
 *  reached the driver's floor is never repeated, so the pair is unique
 *  within a notices list; a recalibration clears the log and starts over. */
function idFor(topic: string, data: Record<string, unknown>): string {
  if (topic === "fan.floorRaised") {
    const from = data.fromRpm ?? data.raisedFromRpm;
    const to = data.toRpm ?? data.raisedToRpm;
    return `fan.floorRaised:${from}-${to}`;
  }
  return `${topic}:${data.seq ?? data.atUnixSecs ?? Math.round(Date.now() / 1000)}`;
}

class Notifications {
  /** Raw records, newest first. */
  private records = $state<StoredEvent[]>([]);
  private readIds = $state<Set<string>>(new Set());

  /** Whether the centre-screen panel is open. Owned here so the header
   *  bell and the panel itself agree without prop-drilling. */
  open = $state(false);

  /** Ids that were unread when the panel was last opened. Opening marks
   *  everything read so the badge clears, but the rows that were new stay
   *  highlighted for that viewing - otherwise "unread" is a state the user
   *  can never actually see. */
  private seenUnread = $state<Set<string>>(new Set());

  /** After a `clear()`, the daemon's persisted log takes a poll or two to
   *  actually empty; ignore its replay until then so cleared notices do
   *  not flash back. `0` outside that window. */
  private replaySuppressedUntil = 0;

  /** The history the panel renders, described in the current language.
   *  `read` is false for a row that was unread when the panel opened, even
   *  once opening marked it read - see `seenUnread`. */
  get list(): AppNotification[] {
    return this.records.map((r) => {
      const describe = DESCRIPTORS[r.topic];
      const parts = describe
        ? describe(r.data)
        : { kind: "info" as const, icon: "info", title: r.topic, body: "" };
      const read = this.readIds.has(r.id) && !this.seenUnread.has(r.id);
      return { id: r.id, topic: r.topic, at: r.at, read, ...parts };
    });
  }

  get unreadCount(): number {
    return this.records.reduce((n, r) => n + (this.readIds.has(r.id) ? 0 : 1), 0);
  }

  /**
   * Loads read-state from the cache and starts following the live event
   * bus. Returns the unsubscribe. Called once, from the layout's
   * `onMount` - not the constructor, which runs at module load where there
   * is no `localStorage`.
   */
  start(): () => void {
    this.loadRead();
    return onDaemonEvent((event) => this.onEvent(event));
  }

  /**
   * Folds the daemon's persisted floor-raise log into the history. Fed
   * from the telemetry poll's `fan.getStatus`, so a raise that happened
   * while the app was closed - or one seen in `vite dev`, where the live
   * bus is a no-op - still turns up within a poll.
   */
  observeFanStatus(status: FanStatus): void {
    if (Date.now() < this.replaySuppressedUntil) return;
    for (const n of status.floorNotices ?? []) {
      const data = {
        fromRpm: n.raisedFromRpm,
        toRpm: n.raisedToRpm,
        stalls: n.stalls,
        reachedDriverFloor: n.reachedDriverFloor,
      };
      this.ingest({
        id: idFor("fan.floorRaised", data),
        topic: "fan.floorRaised",
        at: n.atUnixSecs,
        data,
      });
    }
    this.pruneReadIds();
  }

  /**
   * Pushes a locally-originated notification - one that never came off the
   * daemon's event bus, such as an update check. Each call is its own
   * entry (the id carries the timestamp), so a caller that wants to skip a
   * repeat must decide that itself before calling this - see
   * `settings.notifyUpdateOnce` in `maybeAutoCheckForUpdate`.
   */
  notifyUpdateAvailable(version: string, url: string): void {
    const at = Math.round(Date.now() / 1000);
    const record: StoredEvent = {
      id: `app.updateAvailable:${version}:${at}`,
      topic: "app.updateAvailable",
      at,
      data: { version, url },
    };
    if (this.ingest(record)) void this.notifyOs(record);
  }

  toggle(): void {
    this.open = !this.open;
    if (this.open) {
      // Opening the panel is reading it: the badge clears, but the rows
      // that were new stay marked so the user can see which they are.
      this.seenUnread = new Set(
        this.records.filter((r) => !this.readIds.has(r.id)).map((r) => r.id),
      );
      this.markAllRead();
    }
  }

  close(): void {
    this.open = false;
    this.seenUnread = new Set();
  }

  markAllRead(): void {
    const next = new Set(this.readIds);
    for (const r of this.records) next.add(r.id);
    this.readIds = next;
    this.saveRead();
  }

  /** Drops the whole history, here and in the daemon's own log. */
  clear(): void {
    this.records = [];
    this.readIds = new Set();
    this.seenUnread = new Set();
    this.saveRead();
    this.replaySuppressedUntil = Date.now() + 6000;
    // The daemon persists the fan ones; clearing here without clearing
    // there would bring them all back on the next poll.
    void daemon.clearFloorNotices().catch(() => {
      /* daemon down, or an older build with no such method: the in-app
         list is cleared either way, and a stale persisted copy is a
         cosmetic problem, not a correctness one. */
    });
  }

  private onEvent(event: DaemonEvent): void {
    if (!DESCRIPTORS[event.topic]) return;
    const data = event.payload ?? {};
    const record: StoredEvent = {
      id: idFor(event.topic, data),
      topic: event.topic,
      at: Math.round(Date.now() / 1000 - (event.ageMs ?? 0) / 1000),
      data,
    };
    // Only a genuinely new one is worth an OS notification: the persisted
    // copies replayed on every poll are old news.
    if (this.ingest(record)) void this.notifyOs(record);
  }

  private ingest(record: StoredEvent): boolean {
    if (this.records.some((r) => r.id === record.id)) return false;
    this.records = [record, ...this.records]
      .sort((a, b) => b.at - a.at)
      .slice(0, MAX);
    return true;
  }

  private async notifyOs(record: StoredEvent): Promise<void> {
    const describe = DESCRIPTORS[record.topic];
    if (!describe) return;
    try {
      // Dynamically imported so a browser `vite dev` build never has to
      // resolve the plugin, and so a missing plugin is a caught error
      // rather than a blank page.
      const { isPermissionGranted, requestPermission, sendNotification } = await import(
        "@tauri-apps/plugin-notification"
      );
      let granted = await isPermissionGranted();
      if (!granted) granted = (await requestPermission()) === "granted";
      if (!granted) return;
      const { title, body } = describe(record.data);
      sendNotification({ title, body });
    } catch {
      /* not in Tauri, plugin absent, or permission denied: the in-app
         panel still carries it. */
    }
  }

  private loadRead(): void {
    try {
      const raw = localStorage.getItem(READ_KEY);
      if (raw) this.readIds = new Set(JSON.parse(raw) as string[]);
    } catch {
      /* storage disabled or corrupt: nothing has been read yet */
    }
  }

  private saveRead(): void {
    try {
      localStorage.setItem(READ_KEY, JSON.stringify([...this.readIds]));
    } catch {
      /* private mode / storage off: read-state is a convenience */
    }
  }

  /** Ids that have aged out of the history can never return, so they must
   *  not keep the read set growing forever. */
  private pruneReadIds(): void {
    const live = new Set(this.records.map((r) => r.id));
    if ([...this.readIds].every((id) => live.has(id))) return;
    this.readIds = new Set([...this.readIds].filter((id) => live.has(id)));
    this.saveRead();
  }
}

export const notifications = new Notifications();

/** Whole minutes / hours / days since `unixSecs`, as a catalog string.
 *  Kept here because the panel is the only place that shows one. `t` reads
 *  the current locale, so this re-runs on a language change. */
export function timeAgo(unixSecs: number): string {
  const secs = Math.max(0, Math.round(Date.now() / 1000 - unixSecs));
  if (secs < 60) return t("notifications.age.now");
  const mins = Math.floor(secs / 60);
  if (mins < 60) return t("notifications.age.minutes", { n: mins });
  const hours = Math.floor(mins / 60);
  if (hours < 24) return t("notifications.age.hours", { n: hours });
  return t("notifications.age.days", { n: Math.floor(hours / 24) });
}
