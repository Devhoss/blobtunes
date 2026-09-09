import { describe, it, expect } from "vitest";
import { initialState, queueReducer, type Track } from "../lib/queue";

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

describe("enqueue", () => {
  it("selects first track and appends", () => {
    const s = queueReducer(initialState, { type: "enqueue", track: vod("a") });
    expect(s.items.map((t) => t.id)).toEqual(["a"]);
    expect(s.currentIndex).toBe(0);
  });
  it("does not move selection when appending later tracks", () => {
    let s = queueReducer(initialState, { type: "enqueue", track: vod("a") });
    s = queueReducer(s, { type: "enqueue", track: vod("b") });
    expect(s.items).toHaveLength(2);
    expect(s.currentIndex).toBe(0);
  });
  it("dedupes by stable id", () => {
    let s = queueReducer(initialState, { type: "enqueue", track: vod("a") });
    s = queueReducer(s, { type: "enqueue", track: vod("a") });
    expect(s.items).toHaveLength(1);
  });
  it("accepts live tracks with null duration", () => {
    const s = queueReducer(initialState, { type: "enqueue", track: live("L1") });
    expect(s.items[0].isLive).toBe(true);
    expect(s.items[0].duration).toBeNull();
  });
});

describe("play", () => {
  it("plays a new track immediately while another is playing", () => {
    let s = queueReducer(initialState, { type: "enqueue", track: vod("a") });
    s = queueReducer(s, { type: "play", track: vod("b") });
    expect(s.items.map((t) => t.id)).toEqual(["a", "b"]);
    expect(s.currentIndex).toBe(1); // selected -> player loads it
  });
  it("selects an already-queued track without duplicating", () => {
    let s = queueReducer(initialState, { type: "enqueue", track: vod("a") });
    s = queueReducer(s, { type: "enqueue", track: vod("b") });
    s = queueReducer(s, { type: "play", track: vod("a") });
    expect(s.items).toHaveLength(2);
    expect(s.currentIndex).toBe(0);
  });
  it("plays into an empty queue", () => {
    const s = queueReducer(initialState, { type: "play", track: vod("a") });
    expect(s.items.map((t) => t.id)).toEqual(["a"]);
    expect(s.currentIndex).toBe(0);
  });
});

describe("next/prev semantics", () => {
  it("next clamps at the last track", () => {
    let s = queueReducer(initialState, { type: "enqueue", track: vod("a") });
    s = queueReducer(s, { type: "enqueue", track: vod("b") });
    s = queueReducer(s, { type: "next" });
    expect(s.currentIndex).toBe(1);
    s = queueReducer(s, { type: "next" });
    expect(s.currentIndex).toBe(1); // no wrap
  });
  it("prev clamps at 0", () => {
    const s = { items: [vod("a"), vod("b")], currentIndex: 1 };
    expect(queueReducer(s, { type: "prev" }).currentIndex).toBe(0);
    expect(queueReducer(queueReducer(s, { type: "prev" }), { type: "prev" }).currentIndex).toBe(0);
  });
  it("is safe on empty queue", () => {
    expect(queueReducer(initialState, { type: "next" }).currentIndex).toBe(-1);
    expect(queueReducer(initialState, { type: "prev" }).currentIndex).toBe(-1);
  });
  it("select ignores out-of-range", () => {
    const s = { items: [vod("a")], currentIndex: 0 };
    expect(queueReducer(s, { type: "select", index: 5 }).currentIndex).toBe(0);
  });
});

describe("remove", () => {
  it("keeps selection sane in every position", () => {
    const s0 = { items: [vod("a"), vod("b"), vod("c")], currentIndex: 0 };
    expect(queueReducer(s0, { type: "remove", index: 0 }).currentIndex).toBe(0);
    const afterB = queueReducer({ ...s0, currentIndex: 2 }, { type: "remove", index: 0 });
    expect(afterB.currentIndex).toBe(1); // shifted down
    const s1 = queueReducer(s0, { type: "remove", index: 1 });
    expect(s1.items.map((t) => t.id)).toEqual(["a", "c"]);
    expect(s1.currentIndex).toBe(0);
  });
  it("emptying resets to initialState", () => {
    const s = queueReducer({ items: [vod("a")], currentIndex: 0 }, { type: "remove", index: 0 });
    expect(s).toEqual(initialState);
  });
});

describe("clear", () => {
  it("empties everything", () => {
    expect(queueReducer({ items: [live("x")], currentIndex: 0 }, { type: "clear" })).toEqual(
      initialState,
    );
  });
});
