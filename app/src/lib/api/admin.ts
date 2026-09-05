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
  /** `/proc/acpi/call` is there, which is the only state in which the
   *  fan cleaner and the RGB lightbar work at all. */
  acpiCallLoaded: boolean;
  /** Built for this kernel, whether or not it is loaded. Told apart
   *  because one is a `modprobe` and the other is a package. */
  acpiCallInstalled: boolean;
  /** Whether a polkit agent is available to authenticate a fix. */
  canElevate: boolean;
  /** The pyren-daemon binary, needed to install the service. Null when it
   *  could not be found, in which case that fix is not offered. */
  daemonBinary: string | null;
  user: string | null;
  /** Whether the driver's own library can move a GPU clock offset. When
   *  it can, it needs no X server and no Coolbits, and the Coolbits fix
   *  is beside the point. */
  nvmlOffsets: boolean;
  /** Whether some Xorg config already sets Coolbits. */
  coolbitsSet: boolean;
  /** Whether writing the Coolbits snippet would change anything. False on
   *  a Wayland session, where there is no NVIDIA X screen for it to apply
   *  to — offering it there is a button that changes nothing. */
  coolbitsWouldHelp: boolean;
};

/** The fixes `admin_grant` accepts. Must match `Grant` in admin.rs. */
export type AdminAction =
  | "joinGroup"
  | "installService"
  | "enableService"
  | "loadAcpiCall"
  | "enableCoolbits";

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
