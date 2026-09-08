import type { Track } from "./queue";
import { watchUrl } from "./youtube";

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
    title: item.title,
    channel: item.channel, // V2 contract — every Track carries its channel
    duration: item.is_live ? null : (item.duration ?? null),
    isLive: item.is_live, // HINT ONLY — Task 7 authority rule decides the live display
    thumbnailUrl: item.thumbnail_url || undefined,
  };
}
