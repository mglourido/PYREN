import { beforeEach, describe, expect, it, vi } from "vitest";

// Hoisted above the imports below by Vitest's transform. `telemetry.svelte.ts`
// imports these by the same specifiers (`$lib/api/daemon` directly, the
// others as `./hardware.svelte` etc. - both spellings resolve to the same
// file, which is what Vitest keys the mock on), so mocking them here
// isolates the polling logic from the daemon, from Tauri, and from every
// other store's own side effects.
vi.mock("$lib/api/daemon", () => {
  class DaemonUnavailable extends Error {}
  return {
    DaemonUnavailable,
    daemon: {
      systemMetrics: vi.fn(),
      fanStatus: vi.fn(),
    },
  };
});

vi.mock("$lib/stores/hardware.svelte", () => ({
  hardware: { observeFan: vi.fn() },
}));

vi.mock("$lib/stores/notifications.svelte", () => ({
  notifications: { observeFanStatus: vi.fn() },
}));

vi.mock("$lib/stores/settings.svelte", () => ({
  settings: { current: { pollIntervalMs: 2000 } },
}));

import { daemon } from "$lib/api/daemon";
import { DETAIL_ROUTES, isDetailRoute, Telemetry } from "$lib/stores/telemetry.svelte";

// Partial fixtures cast past their real (much larger) daemon types - these
// tests only exercise the fields `pollOnce`/`applyMetrics` actually read.
const fanFixture = {
  driverInstalled: false,
  cpuTempC: null,
  fanRpm: 0,
  isReverse: false,
} as unknown as Awaited<ReturnType<typeof daemon.fanStatus>>;

const metricsFixture = {
  cpu: { usagePercent: 42, perCorePercent: [42], clocksMhz: [3200], tempC: 55 },
  memory: {
    totalGb: 32,
    usedGb: 8,
    availableGb: 24,
    percent: 25,
    swapTotalGb: 0,
    swapUsedGb: 0,
    ramType: null,
    ramSpeedMts: null,
    ramSlotsUsed: null,
    ramSlotsTotal: null,
    ramEcc: null,
    ramModules: [],
  },
  temperatures: [],
  fans: [],
  disks: [],
  network: { upMbps: 0, downMbps: 0, interfaces: [] },
  gpus: [],
  processes: [],
} as unknown as Awaited<ReturnType<typeof daemon.systemMetrics>>;

beforeEach(() => {
  vi.mocked(daemon.systemMetrics).mockReset();
  vi.mocked(daemon.fanStatus).mockReset().mockResolvedValue(fanFixture);
  // The transition-only console lines in `pollOnce` are irrelevant noise here.
  vi.spyOn(console, "info").mockImplementation(() => {});
  vi.spyOn(console, "warn").mockImplementation(() => {});
});

describe("isDetailRoute", () => {
  it("matches every registered detail route", () => {
    for (const path of DETAIL_ROUTES) {
      expect(isDetailRoute(path)).toBe(true);
    }
  });

  it("rejects a route that only needs reachability, not live metrics", () => {
    expect(isDetailRoute("/settings")).toBe(false);
    expect(isDetailRoute("/system/lighting")).toBe(false);
  });

  it("tolerates a trailing slash", () => {
    expect(isDetailRoute("/system/vitals/")).toBe(true);
  });

  it("matches the root with no trailing slash to strip", () => {
    expect(isDetailRoute("/")).toBe(true);
  });
});

describe("Telemetry polling gate", () => {
  it("skips the heavy metrics call when detail is not active", async () => {
    const telemetry = new Telemetry();
    (telemetry as unknown as { detailActive: boolean }).detailActive = false;

    await (telemetry as unknown as { pollOnce(): Promise<void> }).pollOnce();

    expect(daemon.systemMetrics).not.toHaveBeenCalled();
    expect(daemon.fanStatus).toHaveBeenCalledTimes(1);
  });

  it("calls the heavy metrics endpoint and applies it when detail is active", async () => {
    const telemetry = new Telemetry();
    (telemetry as unknown as { detailActive: boolean }).detailActive = true;
    vi.mocked(daemon.systemMetrics).mockResolvedValue(metricsFixture);

    await (telemetry as unknown as { pollOnce(): Promise<void> }).pollOnce();

    expect(daemon.systemMetrics).toHaveBeenCalledTimes(1);
    expect(telemetry.cpuUsage).toBe(42);
    expect(daemon.fanStatus).toHaveBeenCalledTimes(1);
  });

  it("does not grow history while detail is inactive", async () => {
    const telemetry = new Telemetry();
    (telemetry as unknown as { detailActive: boolean }).detailActive = false;
    const pollOnce = (telemetry as unknown as { pollOnce(): Promise<void> }).pollOnce.bind(
      telemetry,
    );

    await pollOnce();
    const before = telemetry.cpuUsageHistory.length;
    await pollOnce();

    expect(telemetry.cpuUsageHistory.length).toBe(before);
  });

  it("fires an immediate poll when detail turns on", async () => {
    const telemetry = new Telemetry();
    vi.mocked(daemon.systemMetrics).mockResolvedValue(metricsFixture);

    telemetry.setDetailActive(true);

    await vi.waitFor(() => expect(daemon.systemMetrics).toHaveBeenCalledTimes(1));
  });

  it("stays reachable off a fulfilled fanStatus while detail is inactive", async () => {
    const telemetry = new Telemetry();
    (telemetry as unknown as { detailActive: boolean }).detailActive = false;
    vi.mocked(daemon.fanStatus).mockResolvedValue(fanFixture);

    await (telemetry as unknown as { pollOnce(): Promise<void> }).pollOnce();

    expect(telemetry.demo).toBe(false);
  });

  it("falls back to demo off a rejected fanStatus while detail is inactive", async () => {
    const telemetry = new Telemetry();
    (telemetry as unknown as { detailActive: boolean }).detailActive = false;
    vi.mocked(daemon.fanStatus).mockRejectedValue(new Error("daemon unreachable"));

    await (telemetry as unknown as { pollOnce(): Promise<void> }).pollOnce();

    expect(telemetry.demo).toBe(true);
  });
});
