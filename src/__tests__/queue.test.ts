import { describe, it, expect } from "vitest";
import {
  initialState,
  queueReducer,
  pickNextIndex,
  serializeQueue,
  loadQueue,
  QUEUE_CAP,
  type QueueState,
  type Track,
} from "../lib/queue";

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

describe("enqueue_all", () => {
  it("appends in order and selects the first when queue was empty", () => {
    const s = queueReducer(initialState, {
      type: "enqueue_all",
      tracks: [vod("a"), vod("b"), vod("c")],
    });
    expect(s.items.map((t) => t.id)).toEqual(["a", "b", "c"]);
    expect(s.currentIndex).toBe(0);
  });
  it("keeps the current selection when appending to a non-empty queue", () => {
    let s = queueReducer(initialState, { type: "enqueue", track: vod("a") });
    s = queueReducer(s, { type: "play", track: vod("b") });
    s = queueReducer(s, { type: "enqueue_all", tracks: [vod("c"), vod("d")] });
    expect(s.items.map((t) => t.id)).toEqual(["a", "b", "c", "d"]);
    expect(s.currentIndex).toBe(1);
  });
  it("dedupes against the queue and within the batch", () => {
    let s = queueReducer(initialState, { type: "enqueue", track: vod("b") });
    s = queueReducer(s, { type: "enqueue_all", tracks: [vod("a"), vod("b"), vod("a")] });
    expect(s.items.map((t) => t.id)).toEqual(["b", "a"]);
  });
  it("is a no-op when everything already exists", () => {
    const s = queueReducer(initialState, { type: "enqueue", track: vod("a") });
    expect(queueReducer(s, { type: "enqueue_all", tracks: [vod("a")] })).toBe(s);
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

describe("pickNextIndex", () => {
  const items = [vod("a"), vod("b"), vod("c"), vod("d")];
  it("excludes the current track and every played id", () => {
    expect(pickNextIndex(items, ["b"], 0, 0)).toBe(2); // pool = [c, d]
    expect(pickNextIndex(items, ["b"], 0, 0.999)).toBe(3);
  });
  it("clamps out-of-range rolls into the pool", () => {
    expect(pickNextIndex(items, [], 0, 1.5)).toBe(3);
    expect(pickNextIndex(items, [], 0, -1)).toBe(1);
  });
  it("returns -1 when the pool is exhausted", () => {
    expect(pickNextIndex(items, ["b", "c", "d"], 0, 0.5)).toBe(-1);
  });
});

describe("shuffle (hidden pass)", () => {
  const base = (() => {
    let s = queueReducer(initialState, {
      type: "enqueue_all",
      tracks: [vod("a"), vod("b"), vod("c"), vod("d")],
    });
    return { ...s, shuffle: true };
  })();

  it("toggle flips without reordering items", () => {
    const s = queueReducer(base, { type: "toggle_shuffle" });
    expect(s.shuffle).toBeUndefined();
    expect(s.items).toEqual(base.items);
    expect(queueReducer(s, { type: "toggle_shuffle" }).shuffle).toBe(true);
  });
  it("advance picks from the unplayed pool and records the departed track", () => {
    const s = queueReducer(base, { type: "next", roll: 0.999 });
    expect(s.items[s.currentIndex].id).toBe("d");
    expect(s.played).toEqual(["a"]);
  });
  it("every track plays once per pass; an exhausted pool holds when repeat is off", () => {
    let s = queueReducer(base, { type: "next", roll: 0 }); // -> b, played [a]
    s = queueReducer(s, { type: "next", roll: 0 }); // -> c, played [a,b]
    expect(s.items.map((t) => t.id)[s.currentIndex]).toBe("c");
    const held = queueReducer(s, { type: "next", roll: 0 }); // pool = [d] still
    expect(held.items[held.currentIndex].id).toBe("d");
    const exhausted = queueReducer(held, { type: "next", roll: 0 });
    expect(exhausted).toBe(held); // same object: no silent restart
  });
  it("repeat-all (freshPass) starts a new pass with only the departed track marked", () => {
    const allPlayed = { ...base, currentIndex: 3, played: ["a", "b", "c"] };
    const s = queueReducer(allPlayed, { type: "next", freshPass: true, roll: 0 });
    expect(s.items[s.currentIndex].id).toBe("a"); // pool reset, d excluded (departed)
    expect(s.played).toEqual(["d"]);
  });
  it("prev walks the played timeline backwards", () => {
    let s: QueueState = { items: [vod("a"), vod("b"), vod("c")], currentIndex: 2, shuffle: true, played: ["a", "b"] };
    s = queueReducer(s, { type: "prev" });
    expect(s.items[s.currentIndex].id).toBe("b"); // last played, un-marked
    expect(s.played).toEqual(["a"]); // c stays out: stepping back is not a listen
    s = queueReducer(s, { type: "prev" });
    expect(s.items[s.currentIndex].id).toBe("a");
    expect(s.played).toEqual([]);
  });
  it("manual select makes a played track hearable again", () => {
    let s: QueueState = { ...base, played: ["a", "b"], currentIndex: 2 };
    s = queueReducer(s, { type: "select", index: 0 });
    expect(s.currentIndex).toBe(0);
    expect(s.played).toEqual(["b", "c"]); // arrived a un-marked, departed c recorded
  });
  it("linear mode keeps exact prev/next semantics while still recording", () => {
    let s: QueueState = { items: [vod("a"), vod("b"), vod("c")], currentIndex: 0 };
    s = queueReducer(s, { type: "next" });
    expect(s.currentIndex).toBe(1);
    expect(s.played).toEqual(["a"]);
    s = queueReducer(s, { type: "wrap" });
    expect(s.currentIndex).toBe(0);
  });
  it("remove reconciles the played ledger", () => {
    let s: QueueState = { items: [vod("a"), vod("b")], currentIndex: 1, played: ["a"], shuffle: true };
    s = queueReducer(s, { type: "remove", index: 0 });
    expect(s.currentIndex).toBe(0);
    expect(s.played).toEqual([]);
  });
  it("a restored list (currentIndex -1) never auto-selects on enqueue", () => {
    const s = queueReducer({ items: [vod("a")], currentIndex: -1 }, { type: "enqueue", track: vod("b") });
    expect(s.currentIndex).toBe(-1);
    const s2 = queueReducer({ items: [vod("a")], currentIndex: -1 }, {
      type: "enqueue_all",
      tracks: [vod("b")],
    });
    expect(s2.currentIndex).toBe(-1);
  });
  it("clear keeps the shuffle intent but drops the ledger", () => {
    const s = queueReducer(
      { items: [vod("a")], currentIndex: 0, shuffle: true, played: ["a"] },
      { type: "clear" },
    );
    expect(s).toEqual({ items: [], currentIndex: -1, shuffle: true });
  });
});

describe("queue persistence", () => {
  it("round-trips items, shuffle and the played ledger", () => {
    let s = queueReducer(initialState, {
      type: "enqueue_all",
      tracks: [vod("a"), vod("b")],
    });
    s = queueReducer(s, { type: "toggle_shuffle" });
    s = queueReducer(s, { type: "next", roll: 0.5 });
    const back = loadQueue(serializeQueue(s));
    expect(back.items.map((t) => t.id)).toEqual(s.items.map((t) => t.id));
    expect(back.shuffle).toBe(true);
    expect(back.played).toEqual(s.played);
    expect(back.currentIndex).toBe(-1); // list-only resume, always
  });
  it("garbage never throws and always yields a clean empty queue", () => {
    expect(loadQueue(null)).toEqual(initialState);
    expect(loadQueue("not json")).toEqual(initialState);
    expect(loadQueue('{"v":2,"items":[]}')).toEqual(initialState);
    expect(loadQueue('{"v":1,"items":[{"id":"a"}]}')).toEqual(initialState);
    expect(loadQueue('{"v":1}')).toEqual(initialState);
  });
  it("keeps valid rows, drops malformed and duplicate ones", () => {
    const raw = JSON.stringify({
      v: 1,
      items: [
        { id: "a", sourceUrl: "u", title: "t", channel: "c", duration: null, isLive: true },
        { id: "a", sourceUrl: "u", title: "dup", channel: "c", duration: 1, isLive: false },
        { id: "b", sourceUrl: "u", title: "t", channel: "c", duration: 5, isLive: false },
        "junk",
        null,
        42,
      ],
    });
    expect(loadQueue(raw).items.map((t) => t.id)).toEqual(["a", "b"]);
  });
  it("caps a poisoned queue size", () => {
    const items = Array.from({ length: QUEUE_CAP + 100 }, (_, i) => ({
      id: `id${i}`,
      sourceUrl: "u",
      title: "t",
      channel: "c",
      duration: 1,
      isLive: false,
    }));
    expect(loadQueue(JSON.stringify({ v: 1, items })).items).toHaveLength(QUEUE_CAP);
  });
});
