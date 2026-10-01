import { pickNextIndex, type QueueState } from "./queue";

/** Primary-button state for a result row, so the list can never disagree
 * with the dock: the current track shows Pause/Resume (toggle), every
 * other row shows Play (start now). Pure — tested in nowPlaying.test.ts. */
export interface RowPrimaryAction {
  glyph: string;
  label: string;
  mode: "toggle" | "play";
}

export function rowPrimaryAction(opts: {
  isCurrent: boolean;
  playing: boolean;
  paused: boolean;
}): RowPrimaryAction {
  if (!opts.isCurrent) return { glyph: "▶", label: "Play", mode: "play" };
  if (opts.playing && !opts.paused)
    return { glyph: "⏸", label: "Pause", mode: "toggle" };
  if (opts.paused) return { glyph: "▶", label: "Resume", mode: "toggle" };
  return { glyph: "▶", label: "Play", mode: "toggle" };
}

/** Unmute target: the last non-zero volume, or the 80 default when the
 * remembered value is 0/unset (e.g. muted before anything played). */
export function restoreVolume(lastNonZero: number): number {
  return lastNonZero > 0 ? lastNonZero : 80;
}

/** Repeat policy for natural track end. Cycles off -> all -> one -> off.
 * Live tracks are always excluded (plain next): looping a dying HLS feed
 * helps nobody. Under hidden-pass shuffle the "last index" has no meaning
 * (the pass logic in the queue reducer owns exhaustion), so shuffle mode
 * never takes the wrap branch. Pure — tested in nowPlaying.test.ts. */
export type RepeatMode = "off" | "all" | "one";
export type EndedAction = "next" | "wrap" | "reload";

/** Which queue row a background prefetch should resolve next, or null for
 * "nothing worth resolving". Mirrors the advance the queue will actually
 * take, so a prefetch never wastes a dump on a track that won't play:
 *  - repeat-one has no target (the reload is the current, already-cached track);
 *  - shuffle takes the pool head (pure call — does NOT consume the pool;
 *    the advance later commits the id this returned, see queue commitId);
 *  - linear: next row, wrap to 0 only under repeat-all, null at the end;
 *  - live targets are excluded — their HLS URLs are too short-lived to cache.
 * Pure — tested in nowPlaying.test.ts. */
export function prefetchTarget(q: QueueState, mode: RepeatMode): number | null {
  if (q.currentIndex < 0 || q.items.length === 0) return null;
  if (mode === "one") return null;
  let idx: number;
  if (q.shuffle === true) {
    idx = pickNextIndex(q.items, q.played ?? [], q.currentIndex, 0);
  } else if (q.currentIndex + 1 < q.items.length) {
    idx = q.currentIndex + 1;
  } else if (mode === "all") {
    idx = 0;
  } else {
    return null;
  }
  if (idx < 0) return null;
  const target = q.items[idx];
  if (!target || target.isLive) return null;
  return idx;
}

export function nextRepeatMode(m: RepeatMode): RepeatMode {
  return m === "off" ? "all" : m === "all" ? "one" : "off";
}

export function resolveEndedAction(opts: {
  mode: RepeatMode;
  currentIndex: number;
  length: number;
  isLive: boolean;
  shuffle?: boolean;
}): EndedAction {
  if (opts.isLive) return "next";
  if (opts.mode === "one") return "reload";
  if (
    opts.mode === "all" &&
    !opts.shuffle &&
    opts.length > 0 &&
    opts.currentIndex >= opts.length - 1
  )
    return "wrap";
  return "next";
}
