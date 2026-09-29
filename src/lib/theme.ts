/** Blobtunes re-tints itself from the current track: the whole UI hangs off
 * one hue custom property (--h). The hue is a pure function of the YouTube
 * id, so the same song always comes back the same colour and nothing has to
 * be stored. Fallback whenever no track is selected. */

export const DEFAULT_HUE = 285;

/** FNV-1a over the id, folded into a 0-359 hue. */
export function hueOf(id: string | null | undefined): number {
  if (!id) return DEFAULT_HUE;
  let h = 2166136261;
  for (let i = 0; i < id.length; i++) {
    h ^= id.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return (h >>> 0) % 360;
}
