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
  currentIndex: number; // -1 when empty (or restored list-only — see loadQueue)
  /** Hidden-pass shuffle: "next" picks a random track the played timeline
   *  hasn't consumed, and the queue keeps its original order on screen.
   *  Optional so plain {items, currentIndex} literals stay valid. */
  shuffle?: boolean;
  /** Timeline of track ids we have moved AWAY from (oldest first, tail =
   *  most recent). Written even while shuffle is off — it is the same
   *  ledger a future "recently played" view will read. Capped at
   *  PLAYED_CAP so localStorage stays small. */
  played?: string[];
}

export const PLAYED_CAP = 200;
export const QUEUE_CAP = 500;

export type QueueAction =
  | { type: "enqueue"; track: Track }
  | { type: "enqueue_all"; tracks: Track[] } // bulk add (playlist import): order-preserving, dedup by id
  | { type: "play"; track: Track } // enqueue-if-new + select: ▶ means "hear this now"
  | { type: "remove"; index: number }
  | { type: "select"; index: number }
  | { type: "next"; freshPass?: boolean; roll?: number }
  | { type: "wrap"; roll?: number }
  | { type: "prev" }
  | { type: "toggle_shuffle" }
  | { type: "clear" };

export const initialState: QueueState = { items: [], currentIndex: -1 };

/** Random pick from the unplayed pool (current track and every played id
 *  excluded). `roll` is the dice throw (0..1) so the reducer stays pure and
 *  tests are deterministic. -1 = pool exhausted. */
export function pickNextIndex(
  items: Track[],
  played: string[],
  currentIndex: number,
  roll: number,
): number {
  const playedSet = new Set(played);
  const pool: number[] = [];
  for (let i = 0; i < items.length; i++)
    if (i !== currentIndex && !playedSet.has(items[i].id)) pool.push(i);
  if (pool.length === 0) return -1;
  const r = roll < 0 ? 0 : roll >= 1 ? 0.999999 : roll;
  return pool[Math.floor(r * pool.length)];
}

/** The single place selection ever changes: the track we leave is appended
 *  to the played timeline (deduped, capped) and the track we land on is
 *  un-marked — replaying it makes it "unheard" for the shuffle pool again.
 *  `record=false` (prev): stepping BACKWARD is not a listen, so the track
 *  we leave stays out of the ledger — otherwise the next prev would just
 *  bounce back to it. */
function move(s: QueueState, toIndex: number, record = true): QueueState {
  if (toIndex < 0 || toIndex >= s.items.length || toIndex === s.currentIndex) return s;
  const from = s.currentIndex;
  let played = s.played ? [...s.played] : [];
  const departed = record && from >= 0 ? s.items[from]?.id : undefined;
  if (departed) played = played.filter((p) => p !== departed).concat(departed);
  if (played.length > PLAYED_CAP) played = played.slice(played.length - PLAYED_CAP);
  const arrived = s.items[toIndex].id;
  played = played.filter((p) => p !== arrived);
  return {
    ...s,
    currentIndex: toIndex,
    played: played.length ? played : s.played === undefined ? undefined : played,
  };
}

/** Shuffle-mode advance. Pool exhausted: with `freshPass` (repeat-all) the
 *  timeline resets and a new pass begins; otherwise stay put — the same
 *  "queue ran out" silence as linear mode. */
function shuffleAdvance(s: QueueState, freshPass: boolean, roll: number): QueueState {
  const idx = pickNextIndex(s.items, s.played ?? [], s.currentIndex, roll);
  if (idx >= 0) return move(s, idx);
  if (!freshPass) return s;
  const cand = pickNextIndex(s.items, [], s.currentIndex, roll);
  if (cand < 0) return s;
  // New pass: the ledger resets first, so only the departed track counts.
  return move({ ...s, played: undefined }, cand);
}

export function queueReducer(s: QueueState, a: QueueAction): QueueState {
  switch (a.type) {
    case "enqueue":
      if (s.items.some((t) => t.id === a.track.id)) return s;
      return {
        ...s,
        items: [...s.items, a.track],
        // Auto-select only a genuinely fresh queue: a restored (list-only,
        // currentIndex -1) list must not start playing on its own when the
        // user adds something.
        currentIndex: s.currentIndex === -1 && s.items.length === 0 ? 0 : s.currentIndex,
      };
    case "enqueue_all": {
      const seen = new Set(s.items.map((t) => t.id));
      const fresh: Track[] = [];
      for (const t of a.tracks) {
        if (seen.has(t.id)) continue;
        seen.add(t.id);
        fresh.push(t);
      }
      if (fresh.length === 0) return s;
      return {
        ...s,
        items: [...s.items, ...fresh],
        currentIndex: s.currentIndex === -1 && s.items.length === 0 ? 0 : s.currentIndex,
      };
    }
    case "play": {
      const exists = s.items.findIndex((t) => t.id === a.track.id);
      const items = exists >= 0 ? s.items : [...s.items, a.track];
      return move({ ...s, items }, exists >= 0 ? exists : items.length - 1);
    }
    case "remove": {
      const items = s.items.filter((_, i) => i !== a.index);
      let cur = s.currentIndex;
      if (a.index < cur) cur -= 1;
      if (cur >= items.length) cur = items.length - 1;
      const removedId = s.items[a.index]?.id;
      return {
        ...s,
        items,
        currentIndex: cur,
        played: s.played && removedId ? s.played.filter((p) => p !== removedId) : s.played,
      };
    }
    case "select":
      if (a.index < 0 || a.index >= s.items.length) return s;
      return move(s, a.index);
    case "next": {
      if (s.items.length === 0) return s;
      if (s.shuffle) return shuffleAdvance(s, a.freshPass === true, a.roll ?? Math.random());
      return move(s, Math.min(s.currentIndex + 1, s.items.length - 1));
    }
    case "wrap":
      if (s.shuffle) return shuffleAdvance(s, true, a.roll ?? Math.random());
      return move(s, 0);
    case "prev": {
      if (s.items.length === 0) return s;
      if (s.shuffle) {
        const hist = s.played ?? [];
        for (let i = hist.length - 1; i >= 0; i--) {
          const idx = s.items.findIndex((t) => t.id === hist[i]);
          if (idx >= 0 && idx !== s.currentIndex) return move(s, idx, false);
        }
      }
      return move(s, Math.max(s.currentIndex - 1, 0), false);
    }
    case "toggle_shuffle":
      return { ...s, shuffle: s.shuffle === true ? undefined : true };
    case "clear":
      // Clearing is about the LIST — the shuffle intent survives the wipe,
      // the played ledger of vanished tracks does not.
      return { ...initialState, shuffle: s.shuffle };
  }
}

// ---------------------------------------------------------------------------
// Persistence — the queue survives restarts as a LIST only: selection never
// comes back, so the app can never startle you with sound on launch. Lives
// in localStorage beside the repeat/shim keys (same WebView2 profile the
// app already maintains; nothing new is written to disk by our own code).
// ---------------------------------------------------------------------------

export const QUEUE_KEY = "blobtunes:queue";
const QUEUE_VERSION = 1;

export function serializeQueue(s: QueueState): string {
  return JSON.stringify({
    v: QUEUE_VERSION,
    items: s.items,
    shuffle: s.shuffle === true,
    played: s.played ?? [],
  });
}

function validTrack(t: unknown): t is Track {
  if (typeof t !== "object" || t === null) return false;
  const o = t as Record<string, unknown>;
  return (
    typeof o.id === "string" &&
    o.id.length > 0 &&
    typeof o.sourceUrl === "string" &&
    o.sourceUrl.length > 0 &&
    typeof o.title === "string" &&
    typeof o.channel === "string" &&
    typeof o.isLive === "boolean" &&
    (o.duration === null || typeof o.duration === "number") &&
    (o.thumbnailUrl === undefined || typeof o.thumbnailUrl === "string")
  );
}

export function loadQueue(raw: string | null): QueueState {
  if (!raw) return initialState;
  try {
    const o = JSON.parse(raw) as {
      v?: unknown;
      items?: unknown;
      shuffle?: unknown;
      played?: unknown;
    };
    if (o.v !== QUEUE_VERSION || !Array.isArray(o.items)) return initialState;
    const items: Track[] = [];
    const seen = new Set<string>();
    for (const t of o.items) {
      if (!validTrack(t) || seen.has(t.id)) continue;
      seen.add(t.id);
      items.push(t);
      if (items.length >= QUEUE_CAP) break;
    }
    if (items.length === 0) return initialState;
    const played: string[] | undefined = Array.isArray(o.played)
      ? o.played.filter((p): p is string => typeof p === "string").slice(0, PLAYED_CAP)
      : undefined;
    return {
      items,
      currentIndex: -1, // list-only resume, always
      shuffle: o.shuffle === true ? true : undefined,
      played: played && played.length ? played : undefined,
    };
  } catch {
    return initialState;
  }
}
