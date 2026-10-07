import type { NetworkAction } from "$lib/api/daemon";

/** Rule choices in the order the page offers them. */
export const NETWORK_ACTIONS: NetworkAction[] = ["normal", "high", "low", "block"];

/**
 * A byte rate as the bits-per-second figure the page's dial already
 * speaks: the daemon counts bytes, a connection is sold in megabits.
 */
export function formatRate(bytesPerSecond: number): string {
  const bits = Math.max(0, bytesPerSecond) * 8;
  if (bits >= 1_000_000) return `${(bits / 1_000_000).toFixed(2)} Mbps`;
  if (bits >= 1_000) return `${(bits / 1_000).toFixed(0)} kbps`;
  return bits > 0 ? "<1 kbps" : "0 kbps";
}

/**
 * Whether a saved rule is doing nothing right now: `high`/`low` only act
 * while the daemon has `cake` in place to read the class.
 */
export function ruleIsIdle(action: NetworkAction, priorityActive: boolean): boolean {
  return (action === "high" || action === "low") && !priorityActive;
}

/** Linux truncates a process name to 15 bytes; a longer rule never matches. */
export const MAX_PROCESS_NAME = 15;

/**
 * Whether `name` is worth sending to `network.setRule`. The daemon checks
 * the same thing and is the authority; this only decides whether the
 * button is enabled.
 */
export function canAddRule(name: string): boolean {
  const bytes = new TextEncoder().encode(name).length;
  // eslint-disable-next-line no-control-regex
  return bytes > 0 && bytes <= MAX_PROCESS_NAME && !/[\u0000-\u001f\u007f/]/.test(name);
}
