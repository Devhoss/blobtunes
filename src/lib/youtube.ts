const ID_RE = /^[a-zA-Z0-9_-]{11}$/;
const HOST_RE = /^(?:[a-z0-9-]+\.)*youtube\.com$/i;

export function isYouTubeUrl(raw: string): boolean {
  return extractVideoId(raw) !== null;
}

export function extractVideoId(raw: string): string | null {
  let u: URL;
  try {
    u = new URL(raw.trim());
  } catch {
    return null;
  }
  if (u.protocol !== "https:" && u.protocol !== "http:") return null;

  if (u.hostname === "youtu.be") {
    const id = u.pathname.slice(1).split("/")[0];
    return ID_RE.test(id) ? id : null;
  }
  if (!HOST_RE.test(u.hostname)) return null;

  const v = u.searchParams.get("v");
  if (v && ID_RE.test(v)) return v;
  const m = u.pathname.match(/^\/(?:shorts|embed|live|v)\/([a-zA-Z0-9_-]{11})(?:\/|$)/);
  return m ? m[1] : null;
}

export function watchUrl(id: string): string {
  return `https://www.youtube.com/watch?v=${id}`;
}
