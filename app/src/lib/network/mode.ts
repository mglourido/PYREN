import type { NetworkStatus } from "$lib/api/daemon";

export function networkDescriptionKey(mode: NetworkStatus["mode"]) {
  if (mode === "off") return "network.descOff";
  if (mode === "auto") return "network.descAuto";
  return "network.descUnknown";
}

export function networkSummaryKey(mode: NetworkStatus["mode"]) {
  if (mode === "off") return "common.disabled";
  if (mode === "auto") return "common.enabled";
  return "system.unknown";
}
