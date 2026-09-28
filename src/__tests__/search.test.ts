import { describe, it, expect } from "vitest";
import { toTrack, type SearchItem } from "../lib/search";

const base: SearchItem = {
  id: "abc123XYZ_-",
  title: "Song A",
  channel: "Chan",
  thumbnail_url: "https://i.ytimg.com/vi/abc123XYZ_-/mqdefault.jpg",
  duration: 192,
  is_live: false,
};

describe("toTrack normalization", () => {
  it("maps a VOD result to a Track", () => {
    const t = toTrack(base);
    expect(t.id).toBe("abc123XYZ_-");
    expect(t.sourceUrl).toBe("https://www.youtube.com/watch?v=abc123XYZ_-");
    expect(t.channel).toBe("Chan");
    expect(t.duration).toBe(192);
    expect(t.isLive).toBe(false);
  });
  it("live result: duration forced null, isLive true", () => {
    const t = toTrack({ ...base, is_live: true, duration: 0 });
    expect(t.duration).toBeNull();
    expect(t.isLive).toBe(true);
  });
  it("unknown VOD duration stays null, not 0", () => {
    const t = toTrack({ ...base, duration: null });
    expect(t.duration).toBeNull();
  });
  it("decodes HTML entities in YouTube titles", () => {
    const t = toTrack({ ...base, title: "90&#39;s Chill &amp; LoFi" });
    expect(t.title).toBe("90's Chill & LoFi");
  });
  it("missing thumbnail becomes undefined", () => {
    expect(toTrack({ ...base, thumbnail_url: "" }).thumbnailUrl).toBeUndefined();
  });
});
