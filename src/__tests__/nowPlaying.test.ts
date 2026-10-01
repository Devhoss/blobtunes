import { describe, it, expect } from "vitest";
import { rowPrimaryAction, restoreVolume } from "../lib/nowPlaying";
import {
  nextRepeatMode,
  prefetchTarget,
  resolveEndedAction,
} from "../lib/nowPlaying";
import { type QueueState, type Track } from "../lib/queue";

const vod = (id: string): Track => ({
  id,
  sourceUrl: `https://www.youtube.com/watch?v=${id}`,
  title: id,
  channel: "Chan",
  duration: 100,
  isLive: false,
});
const live = (id: string): Track => ({
  id,
  sourceUrl: `https://www.youtube.com/watch?v=${id}`,
  title: id,
  channel: "Chan",
  duration: null,
  isLive: true,
});

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

describe("prefetchTarget", () => {
  it("linear mode targets the row after the current one", () => {
    const q: QueueState = { items: [vod("a"), vod("b"), vod("c")], currentIndex: 1 };
    expect(prefetchTarget(q, "off")).toBe(2);
  });
  it("linear end: repeat-off has no target, repeat-all wraps to 0", () => {
    const q: QueueState = { items: [vod("a"), vod("b")], currentIndex: 1 };
    expect(prefetchTarget(q, "off")).toBeNull();
    expect(prefetchTarget(q, "all")).toBe(0);
  });
  it("repeat-one has no target — the reload is the current (cached) track", () => {
    const q: QueueState = { items: [vod("a"), vod("b")], currentIndex: 0 };
    expect(prefetchTarget(q, "one")).toBeNull();
  });
  it("live targets are never prefetched (URLs too short-lived to cache)", () => {
    const q: QueueState = { items: [vod("a"), live("L")], currentIndex: 0 };
    expect(prefetchTarget(q, "off")).toBeNull();
  });
  it("shuffle mode targets an unplayed, non-current track", () => {
    const q: QueueState = {
      items: [vod("a"), vod("b"), vod("c"), vod("d")],
      currentIndex: 0,
      shuffle: true,
      played: ["a", "c"],
    };
    expect(prefetchTarget(q, "off")).toBe(1); // pool [b,d], deterministic roll
  });
  it("shuffle mode is side-effect free: no pool consumption, no mutation", () => {
    const q: QueueState = {
      items: [vod("a"), vod("b"), vod("c")],
      currentIndex: 0,
      shuffle: true,
      played: ["a"],
    };
    const snapshot = JSON.parse(JSON.stringify(q));
    prefetchTarget(q, "off");
    prefetchTarget(q, "off");
    expect(q).toEqual(snapshot);
  });
  it("exhausted shuffle pool only leaves the current track: no target", () => {
    const q: QueueState = {
      items: [vod("a"), vod("b")],
      currentIndex: 0,
      shuffle: true,
      played: ["b"],
    };
    expect(prefetchTarget(q, "all")).toBeNull();
  });
  it("no selection (restored list) has no target", () => {
    const q: QueueState = { items: [vod("a"), vod("b")], currentIndex: -1 };
    expect(prefetchTarget(q, "off")).toBeNull();
  });
});
