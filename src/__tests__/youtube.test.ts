import { describe, it, expect } from "vitest";
import { extractVideoId, isYouTubeUrl, watchUrl } from "../lib/youtube";

describe("isYouTubeUrl", () => {
  it("accepts all supported public URL forms", () => {
    const ok = [
      "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
      "https://youtu.be/dQw4w9WgXcQ?t=42",
      "https://m.youtube.com/watch?v=dQw4w9WgXcQ",
      "https://music.youtube.com/watch?v=dQw4w9WgXcQ",
      "https://www.youtube.com/shorts/dQw4w9WgXcQ",
      "https://www.youtube.com/embed/dQw4w9WgXcQ",
      "https://www.youtube.com/live/dQw4w9WgXcQ",
      "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=RDxyz&t=3",
      "http://www.youtube.com/watch?v=dQw4w9WgXcQ",
    ];
    for (const u of ok) expect(isYouTubeUrl(u), u).toBe(true);
  });
  it("rejects junk", () => {
    for (const u of [
      "https://example.com/watch?v=dQw4w9WgXcQ",
      "not a url",
      "",
      "https://www.youtube.com/feed/subscriptions",
      "https://www.youtube.com/watch?v=short",
      "https://youtu.be/",
    ]) expect(isYouTubeUrl(u), u).toBe(false);
  });
});

describe("extractVideoId", () => {
  it("gets the 11-char id from every accepted form", () => {
    const pairs = [
      ["https://www.youtube.com/watch?v=dQw4w9WgXcQ", "dQw4w9WgXcQ"],
      ["https://youtu.be/dQw4w9WgXcQ?si=abc", "dQw4w9WgXcQ"],
      ["https://music.youtube.com/watch?v=dQw4w9WgXcQ", "dQw4w9WgXcQ"],
      ["https://www.youtube.com/shorts/dQw4w9WgXcQ", "dQw4w9WgXcQ"],
      ["https://www.youtube.com/embed/dQw4w9WgXcQ", "dQw4w9WgXcQ"],
      ["https://www.youtube.com/live/dQw4w9WgXcQ", "dQw4w9WgXcQ"],
    ];
    for (const [u, id] of pairs) expect(extractVideoId(u), u).toBe(id);
  });
  it("returns null for non-YouTube / malformed input", () => {
    expect(extractVideoId("https://example.com/watch?v=dQw4w9WgXcQ")).toBeNull();
    expect(extractVideoId("https://youtube.com/watch?v=xyz")).toBeNull();
    expect(extractVideoId("")).toBeNull();
  });
});

describe("watchUrl", () => {
  it("builds the canonical watch URL", () => {
    expect(watchUrl("dQw4w9WgXcQ")).toBe("https://www.youtube.com/watch?v=dQw4w9WgXcQ");
  });
});
