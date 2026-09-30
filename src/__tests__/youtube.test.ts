import { describe, it, expect } from "vitest";
import {
  extractPlaylistId,
  extractVideoId,
  isYouTubeUrl,
  watchUrl,
} from "../lib/youtube";

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
      "https://www.youtube.com/playlist?list=PLGwRdxNW4CGoR0FtOEQGGmo5N71WpKT3R",
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
      "https://www.youtube.com/playlist",
      "https://www.youtube.com/playlist?list=short",
      "https://example.com/playlist?list=PLGwRdxNW4CGoR0FtOEQGGmo5N71WpKT3R",
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

describe("extractPlaylistId", () => {
  it("gets the list id from playlist URLs", () => {
    const pairs = [
      [
        "https://www.youtube.com/playlist?list=PLGwRdxNW4CGoR0FtOEQGGmo5N71WpKT3R",
        "PLGwRdxNW4CGoR0FtOEQGGmo5N71WpKT3R",
      ],
      [
        "http://youtube.com/playlist?list=RDMMdQw4w9WgXcQ",
        "RDMMdQw4w9WgXcQ",
      ],
      [
        "https://music.youtube.com/playlist?list=OLAK5uy_l3VYUb8jhP5ZoOg2fWDoX42XwcX3tpFbM&si=abc",
        "OLAK5uy_l3VYUb8jhP5ZoOg2fWDoX42XwcX3tpFbM",
      ],
      [
        "https://www.youtube.com/playlist?list=PLGwRdxNW4CGoR0FtOEQGGmo5N71WpKT3R ",
        "PLGwRdxNW4CGoR0FtOEQGGmo5N71WpKT3R",
      ],
    ];
    for (const [u, id] of pairs) expect(extractPlaylistId(u), u).toBe(id);
  });
  it("returns null for everything else", () => {
    const no = [
      "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLGwRdxNW4CGoR0FtOEQGGmo5N71WpKT3R",
      "https://www.youtube.com/playlist",
      "https://www.youtube.com/playlist?list=short",
      "https://youtu.be/dQw4w9WgXcQ?list=PLGwRdxNW4CGoR0FtOEQGGmo5N71WpKT3R",
      "https://example.com/playlist?list=PLGwRdxNW4CGoR0FtOEQGGmo5N71WpKT3R",
      "not a url",
      "",
    ];
    for (const u of no) expect(extractPlaylistId(u), u).toBeNull();
  });
  it("watch?v= URLs keep single-video precedence", () => {
    // Mixed URLs must NOT flip to playlist mode: extractVideoId still wins
    // and extractPlaylistId stays null.
    const mixed =
      "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLGwRdxNW4CGoR0FtOEQGGmo5N71WpKT3R";
    expect(extractVideoId(mixed)).toBe("dQw4w9WgXcQ");
    expect(extractPlaylistId(mixed)).toBeNull();
  });
});
