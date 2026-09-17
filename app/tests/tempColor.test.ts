import { describe, expect, it } from "vitest";
import { tempColor } from "$lib/stores/telemetry.svelte";

describe("tempColor", () => {
  it("returns the muted color for an unknown reading", () => {
    expect(tempColor(null)).toBe("var(--text-mute)");
  });

  it("returns danger at or above 90", () => {
    expect(tempColor(90)).toBe("var(--danger)");
  });

  it("returns warn at or above 75 but below 90", () => {
    expect(tempColor(75)).toBe("var(--warn)");
    expect(tempColor(89.9)).toBe("var(--warn)");
  });

  it("returns ok below 75", () => {
    expect(tempColor(74.9)).toBe("var(--ok)");
  });
});
