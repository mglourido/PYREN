import { describe, expect, it } from "vitest";
import { networkDescriptionKey, networkSummaryKey } from "../src/lib/network/mode";

describe("network mode presentation", () => {
  it("does not label an unknown observation as Auto or enabled", () => {
    expect(networkDescriptionKey(null)).toBe("network.descUnknown");
    expect(networkSummaryKey(null)).toBe("system.unknown");
  });

  it("keeps known modes distinct", () => {
    expect(networkDescriptionKey("off")).toBe("network.descOff");
    expect(networkDescriptionKey("auto")).toBe("network.descAuto");
    expect(networkSummaryKey("off")).toBe("common.disabled");
    expect(networkSummaryKey("auto")).toBe("common.enabled");
  });
});
