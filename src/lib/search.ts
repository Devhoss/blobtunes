import type { Track } from "./queue";
import { watchUrl } from "./youtube";

const HTML_ENTITIES: Record<string, string> = {
  amp: "&",
  apos: "'",
  gt: ">",
  lt: "<",
  nbsp: " ",
  quot: '"',
};

/** Decode the HTML entities YouTube includes in some API titles. */
function decodeHtmlEntities(value: string): string {
  return value.replace(
    /&(?:#(x[0-9a-f]+|[0-9]+)|([a-z][a-z0-9]+));/gi,
    (entity, numeric: string | undefined, named: string | undefined) => {
      if (named) {
        return HTML_ENTITIES[named.toLowerCase()] ?? entity;
      }

      if (!numeric) {
        return entity;
      }

      const radix = numeric.toLowerCase().startsWith("x") ? 16 : 10;
      const codePoint = Number.parseInt(numeric.replace(/^x/i, ""), radix);
      if (!Number.isFinite(codePoint) || codePoint <= 0 || codePoint > 0x10ffff) {
        return entity;
      }

      try {
        return String.fromCodePoint(codePoint);
      } catch {
        return entity;
      }
    },
  );
}

/** Shape returned by the Rust `search_youtube` command. */
export interface SearchItem {
  id: string;
  title: string;
  channel: string;
  thumbnail_url: string;
  duration: number | null;
  is_live: boolean;
}

/** Normalize a search result into the queue's Track model. */
export function toTrack(item: SearchItem): Track {
  return {
    id: item.id,
    sourceUrl: watchUrl(item.id),
    title: decodeHtmlEntities(item.title),
    channel: item.channel, // V2 contract — every Track carries its channel
    duration: item.is_live ? null : (item.duration ?? null),
    isLive: item.is_live, // HINT ONLY — Task 7 authority rule decides the live display
    thumbnailUrl: item.thumbnail_url || undefined,
  };
}
