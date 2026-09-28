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
 * helps nobody. Pure — tested in nowPlaying.test.ts. */
export type RepeatMode = "off" | "all" | "one";
export type EndedAction = "next" | "wrap" | "reload";

export function nextRepeatMode(m: RepeatMode): RepeatMode {
  return m === "off" ? "all" : m === "all" ? "one" : "off";
}

export function resolveEndedAction(opts: {
  mode: RepeatMode;
  currentIndex: number;
  length: number;
  isLive: boolean;
}): EndedAction {
  if (opts.isLive) return "next";
  if (opts.mode === "one") return "reload";
  if (
    opts.mode === "all" &&
    opts.length > 0 &&
    opts.currentIndex >= opts.length - 1
  )
    return "wrap";
  return "next";
}
