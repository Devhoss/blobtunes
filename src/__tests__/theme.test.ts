import { describe, it, expect } from "vitest";
import { DEFAULT_HUE, hueOf } from "../lib/theme";

describe("hueOf", () => {
  it("falls back to the default hue with no track", () => {
    expect(hueOf(null)).toBe(DEFAULT_HUE);
    expect(hueOf(undefined)).toBe(DEFAULT_HUE);
    expect(hueOf("")).toBe(DEFAULT_HUE);
  });

  it("is stable for a given id", () => {
    expect(hueOf("dQw4w9WgXcQ")).toBe(hueOf("dQw4w9WgXcQ"));
  });

  it("maps the catalog ids to their pinned hues", () => {
    // Pinned so a re-tint can never silently shift every track's colour.
    expect(hueOf("dQw4w9WgXcQ")).toBe(37);
    expect(hueOf("kJQP7kiw5Fk")).toBe(101);
    expect(hueOf("jNQXAC9IVRw")).toBe(307);
    expect(hueOf("abc123XYZ_-")).toBe(128);
  });

  it("always lands inside 0-359", () => {
    for (const id of ["a", "b", "zzzzzzzzzzz", "9bZkp7q19f0", "fJ9rUzIMcZQ"]) {
      const h = hueOf(id);
      expect(Number.isInteger(h)).toBe(true);
      expect(h).toBeGreaterThanOrEqual(0);
      expect(h).toBeLessThan(360);
    }
  });

  it("gives neighbouring ids different hues", () => {
    expect(hueOf("a")).not.toBe(hueOf("b"));
  });
});
