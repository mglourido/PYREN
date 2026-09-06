/**
 * The processes Pyren needs running, and what starts at login.
 *
 * Answered by the Tauri shell rather than the daemon (see
 * `src-tauri/src/session.rs`), for the same reason as `admin`: one of the
 * things being reported is whether the daemon is there at all.
 *
 * The app already starts the widget by itself at launch. Everything here
 * is for the settings page: saying what is running, and offering the two
 * choices that outlive this window - the widget at login, and the app at
 * login. Neither needs a password; both write only under
 * `~/.config`.
 */

import { invoke } from "@tauri-apps/api/core";
import { inTauri } from "./daemon";

export type SessionStatus = {
  osd: {
    /** A `pyren-osd` belonging to this user is up right now. */
    running: boolean;
    /** Where its binary was found, or null when it is not installed. */
    binary: string | null;
    /** `~/.config/systemd/user/pyren-osd.service` exists. */
    unitInstalled: boolean;
    /** Its service unit or its `.path` watcher is enabled. The watcher is
     *  what fires on a compositor that never reaches
     *  `graphical-session.target`, so the widget needs no `loginWorks`
     *  caveat the way the app does. */
    startsAtLogin: boolean;
  };
  app: {
    startsAtLogin: boolean;
    /** The XDG autostart entry this toggle writes. */
    entry: string;
    /** …and the systemd user unit it writes alongside it. */
    unit: string;
    /** The line that starts the app where `loginWorks` is false. */
    loginCommand: string;
  };
  /**
   * Whether anything in this session acts on an autostart entry or reaches
   * `graphical-session.target`. False on a bare wlroots compositor
   * (Hyprland, Sway), which has neither — both "at login" toggles then write
   * correct files that nobody reads, which is why it is said out loud rather
   * than left looking like two switches that do nothing.
   *
   * Says nothing about the daemon: that is a system service started at boot,
   * long before any compositor.
   */
  loginWorks: boolean;
};

export const session = {
  status: () => invoke<SessionStatus>("session_status"),
  /** Starts the widget now — for when it was stopped by hand. */
  startOsd: () => invoke<SessionStatus>("session_start_osd"),
  /** Shows the widget without touching the power mode — the widget's own
   *  "open the mode switcher" path, not the key's cycle. */
  showOsd: () => invoke<SessionStatus>("session_show_osd"),
  /** Stops it now *and* stops it starting at login: switching the widget
   *  off has to mean off, not "until tomorrow". */
  stopOsd: () => invoke<SessionStatus>("session_stop_osd"),
  setOsdAtLogin: (enabled: boolean) =>
    invoke<SessionStatus>("session_set_osd_at_login", { enabled }),
  setAppAtLogin: (enabled: boolean) =>
    invoke<SessionStatus>("session_set_app_at_login", { enabled }),
  /** False in a plain browser tab, where there is no session to manage. */
  available: () => inTauri,
};
