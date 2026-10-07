import { describe, expect, it } from "vitest";
import { canAddRule, formatRate, ruleIsIdle } from "../src/lib/network/processes";

describe("per-process network presentation", () => {
  it("shows byte rates as bits per second", () => {
    expect(formatRate(0)).toBe("0 kbps");
    expect(formatRate(50)).toBe("<1 kbps");
    expect(formatRate(125_000)).toBe("1.00 Mbps");
    expect(formatRate(12_500)).toBe("100 kbps");
    expect(formatRate(1_562_500)).toBe("12.50 Mbps");
  });

  it("never shows a negative rate", () => {
    expect(formatRate(-10)).toBe("0 kbps");
  });

  it("flags priority rules as idle only without an active queue", () => {
    expect(ruleIsIdle("high", false)).toBe(true);
    expect(ruleIsIdle("low", false)).toBe(true);
    expect(ruleIsIdle("high", true)).toBe(false);
    expect(ruleIsIdle("block", false)).toBe(false);
    expect(ruleIsIdle("normal", false)).toBe(false);
  });

  it("only offers names that could be a process", () => {
    expect(canAddRule("steam")).toBe(true);
    expect(canAddRule("Web Content")).toBe(true);
    expect(canAddRule("exactly15chars.")).toBe(true);
    expect(canAddRule("")).toBe(false);
    expect(canAddRule("sixteen-chars-xx")).toBe(false);
    expect(canAddRule("a/b")).toBe(false);
    // Fifteen characters, but more than fifteen bytes.
    expect(canAddRule("ññññññññññ")).toBe(false);
  });
});
