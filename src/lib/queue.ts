export interface Track {
  id: string;
  sourceUrl: string;
  title: string;
  channel: string; // V2 contract — shown under every title; from search snippet or probe
  duration: number | null; // null = unknown or live
  isLive: boolean; // HINT ONLY — active-track liveness comes from player state (Task 7 authority rule)
  thumbnailUrl?: string;
}

export interface QueueState {
  items: Track[];
  currentIndex: number; // -1 when empty
}

export type QueueAction =
  | { type: "enqueue"; track: Track }
  | { type: "remove"; index: number }
  | { type: "select"; index: number }
  | { type: "next" }
  | { type: "prev" }
  | { type: "clear" };

export const initialState: QueueState = { items: [], currentIndex: -1 };

export function queueReducer(s: QueueState, a: QueueAction): QueueState {
  switch (a.type) {
    case "enqueue":
      if (s.items.some((t) => t.id === a.track.id)) return s;
      return {
        items: [...s.items, a.track],
        currentIndex: s.currentIndex === -1 ? 0 : s.currentIndex,
      };
    case "remove": {
      const items = s.items.filter((_, i) => i !== a.index);
      let cur = s.currentIndex;
      if (a.index < cur) cur -= 1;
      if (cur >= items.length) cur = items.length - 1;
      return { items, currentIndex: cur };
    }
    case "select":
      return a.index < 0 || a.index >= s.items.length ? s : { ...s, currentIndex: a.index };
    case "next":
      return s.items.length === 0
        ? s
        : { ...s, currentIndex: Math.min(s.currentIndex + 1, s.items.length - 1) };
    case "prev":
      return s.items.length === 0
        ? s
        : { ...s, currentIndex: Math.max(s.currentIndex - 1, 0) };
    case "clear":
      return initialState;
  }
}
