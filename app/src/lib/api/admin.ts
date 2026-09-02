/**
 * Admin mode: the privileges the app needs, and the fixes for the ones it
 * hasn't got.
 *
 * Everything here is answered by the Tauri shell itself rather than by the
 * daemon, because "the daemon is unreachable" is precisely one of the
 * states being diagnosed. See `src-tauri/src/admin.rs`.
 */

import { invoke } from "@tauri-apps/api/core";
import { inTauri } from "./daemon";

export type AdminStatus = {
  socketPath: string;
  socketReachable: boolean;
  /** Reachable but refused: the user is not in the socket's group. */
  socketDenied: boolean;
  /** Where the systemd unit was found, or null when it isn't installed. */
  unitPath: string | null;
  serviceActive: boolean;
  serviceEnabled: boolean;
  groupName: string;
  groupExists: boolean;
  /** A member according to the group database. */
  inGroupDatabase: boolean;
  /** A member according to this login session, which is what actually
   *  decides whether the socket opens. */
  sessionHasGroup: boolean;
  /** Member on paper only: nothing works until the user logs back in. */
  needsRelogin: boolean;
  /** Whether a polkit agent is available to authenticate a fix. */
  canElevate: boolean;
  /** The pyren-daemon binary, needed to install the service. Null when it
   *  could not be found, in which case that fix is not offered. */
  daemonBinary: string | null;
  user: string | null;
};

/** The fixes `admin_grant` accepts. Must match `Grant` in admin.rs. */
export type AdminAction = "joinGroup" | "installService" | "enableService";

export type GrantResult = {
  applied: boolean;
  /** The user dismissed the authentication dialog. Not an error. */
  cancelled: boolean;
  detail: string;
};

export const admin = {
  status: () => invoke<AdminStatus>("admin_status"),
  grant: (action: AdminAction) => invoke<GrantResult>("admin_grant", { action }),
  /** False in a plain browser tab, where there is no shell to ask. */
  available: () => inTauri,
};
