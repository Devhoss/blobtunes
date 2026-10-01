import { describe, it, expect } from "vitest";
import { rowPrimaryAction, restoreVolume } from "../lib/nowPlaying";
import {
  nextRepeatMode,
  resolveEndedAction,
} from "../lib/nowPlaying";

describe("rowPrimaryAction", () => {
  it("non-current row offers Play", () => {
    expect(
      rowPrimaryAction({ isCurrent: false, playing: true, paused: false }),
    ).toEqual({ glyph: "▶", label: "Play", mode: "play" });
  });
  it("current + sounding row offers Pause via toggle", () => {
    expect(
      rowPrimaryAction({ isCurrent: true, playing: true, paused: false }),
    ).toEqual({ glyph: "⏸", label: "Pause", mode: "toggle" });
  });
  it("current + paused row offers Resume via toggle", () => {
    expect(
      rowPrimaryAction({ isCurrent: true, playing: true, paused: true }),
    ).toEqual({ glyph: "▶", label: "Resume", mode: "toggle" });
  });
  it("current + not-yet-playing row offers Play via toggle", () => {
    expect(
      rowPrimaryAction({ isCurrent: true, playing: false, paused: false }),
    ).toEqual({ glyph: "▶", label: "Play", mode: "toggle" });
  });
});

describe("restoreVolume", () => {
  it("restores the last non-zero volume on unmute", () => {
    expect(restoreVolume(42)).toBe(42);
  });
  it("falls back to 80 when nothing remembered", () => {
    expect(restoreVolume(0)).toBe(80);
  });
});

describe("repeat policy", () => {
  it("cycles off -> all -> one -> off", () => {
    expect(nextRepeatMode("off")).toBe("all");
    expect(nextRepeatMode("all")).toBe("one");
    expect(nextRepeatMode("one")).toBe("off");
  });
  it("off advances without wrapping", () => {
    expect(
      resolveEndedAction({ mode: "off", currentIndex: 2, length: 3, isLive: false }),
    ).toBe("next");
  });
  it("all wraps at the last track", () => {
    expect(
      resolveEndedAction({ mode: "all", currentIndex: 2, length: 3, isLive: false }),
    ).toBe("wrap");
  });
  it("all advances mid-queue", () => {
    expect(
      resolveEndedAction({ mode: "all", currentIndex: 0, length: 3, isLive: false }),
    ).toBe("next");
  });
  it("one reloads the current track", () => {
    expect(
      resolveEndedAction({ mode: "one", currentIndex: 0, length: 3, isLive: false }),
    ).toBe("reload");
  });
  it("live is excluded from every repeat mode", () => {
    expect(
      resolveEndedAction({ mode: "one", currentIndex: 0, length: 3, isLive: true }),
    ).toBe("next");
    expect(
      resolveEndedAction({ mode: "all", currentIndex: 2, length: 3, isLive: true }),
    ).toBe("next");
  });
  it("shuffle mode never wraps — the pass logic owns exhaustion", () => {
    expect(
      resolveEndedAction({
        mode: "all",
        currentIndex: 2,
        length: 3,
        isLive: false,
        shuffle: true,
      }),
    ).toBe("next");
  });
});
