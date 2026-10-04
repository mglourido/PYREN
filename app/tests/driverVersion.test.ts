import { beforeEach, describe, expect, it, vi } from "vitest";

// The same isolation as `telemetry.polling.test.ts`: the store is tested
// against a daemon that answers whatever the test tells it to.
vi.mock("$lib/api/daemon", () => {
  class DaemonUnavailable extends Error {}
  return {
    DaemonUnavailable,
    daemon: {
      installerInspect: vi.fn(),
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

import { daemon, type DriverVersion } from "$lib/api/daemon";
import { Telemetry } from "$lib/stores/telemetry.svelte";

const outdated: DriverVersion = {
  state: "outdated",
  outdated: true,
  installed: { sha256: "2eab8333".padEnd(64, "0"), label: null },
  installedFrom: "source",
  bundled: { sha256: "4d82cbb6".padEnd(64, "0"), label: "2d3f2a4 (2026-10-04)" },
};

type Inspection = Awaited<ReturnType<typeof daemon.installerInspect>>;

beforeEach(() => {
  vi.mocked(daemon.installerInspect).mockReset();
});

describe("Telemetry.loadDriverVersion", () => {
  it("keeps the daemon's verdict for the shell's notice to read", async () => {
    vi.mocked(daemon.installerInspect).mockResolvedValue({
      driverVersion: outdated,
    } as unknown as Inspection);
    const telemetry = new Telemetry();

    await telemetry.loadDriverVersion();

    expect(telemetry.driverVersion).toEqual(outdated);
  });

  it("has no verdict, rather than a stale one, when the daemon cannot be asked", async () => {
    const telemetry = new Telemetry();
    telemetry.driverVersion = outdated;
    vi.mocked(daemon.installerInspect).mockRejectedValue(new Error("daemon is down"));

    await telemetry.loadDriverVersion();

    expect(telemetry.driverVersion).toBeNull();
  });

  // An app updated ahead of its daemon talks to one that has never heard
  // of `driverVersion`. That is "not known", not a crash and not a notice.
  it("has no verdict from a daemon that predates the field", async () => {
    vi.mocked(daemon.installerInspect).mockResolvedValue({
      environment: {},
      patchNeeded: false,
    } as unknown as Inspection);
    const telemetry = new Telemetry();

    await telemetry.loadDriverVersion();

    expect(telemetry.driverVersion).toBeNull();
  });
});
