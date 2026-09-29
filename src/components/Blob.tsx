import { useEffect, useRef } from "react";

/** Canvas backing size. The CSS box is --bs (172px, 64px in dense/mini); every
 * coordinate below is in this space and scaled to the element by the browser. */
const SIZE = 420;

/** The mascot only fills the middle ~47% of the canvas, so at the design's
 * dense/mini size it reads as a dot. Dense draws it this much larger instead. */
const DENSE_ZOOM = 1.6;

export interface BlobProps {
  /** Sounding right now (not paused, not merely loading). */
  playing: boolean;
  /** A track is selected at all. Drives the sleepy face and the z's. */
  hasTrack: boolean;
  /** Theme hue in degrees; the blob is the biggest block of colour. */
  hue: number;
  /** Dense header or mini: the 64px box needs the mascot drawn zoomed in. */
  dense?: boolean;
  onToggle: () => void;
}

interface Point {
  x: number;
  y: number;
}

/** One frame of the mascot: orbiting specks, a wobbling body, eyes that track
 * the pointer, and a mouth that answers playback state. */
function draw(
  c: HTMLCanvasElement | null,
  t: number,
  playing: boolean,
  hasTrack: boolean,
  hue: number,
  p: Point,
  beatRef: { current: number },
  zoom: number,
): void {
  if (!c) return;
  const g = c.getContext("2d");
  if (!g) return;

  const cx = 210,
    cy = 214;
  const zoomed = zoom !== 1;
  const c1 = `hsl(${hue} 95% 76%)`,
    c2 = `hsl(${hue} 90% 60%)`,
    c3 = `hsl(${(hue + 60) % 360} 85% 46%)`;

  g.clearRect(0, 0, SIZE, SIZE);
  // Zoom about the mascot's own centre: the specks orbit further out than the
  // scaled body and would be the first thing to leave the canvas, so they are
  // dropped entirely when zoomed (the design does the same).
  if (zoomed) {
    g.save();
    g.translate(cx, cy);
    g.scale(zoom, zoom);
    g.translate(-cx, -cy);
  }

  // Synthetic heartbeat: no audio tap exists (mpv owns the stream), so the
  // pulse is a pair of sines that only run while something is sounding.
  const target = playing
    ? (0.5 + 0.5 * Math.sin(t / 230)) * (0.6 + 0.4 * Math.sin(t / 810))
    : 0.04;
  beatRef.current += (target - beatRef.current) * 0.18;
  const beat = beatRef.current;

  for (let k = 0; k < (zoomed ? 0 : 7); k++) {
    const a = t / (1400 - k * 70) + k * 0.9;
    const r = 138 + Math.sin(t / 600 + k) * 8;
    g.fillStyle = k % 2 ? c1 : c2;
    g.globalAlpha = 0.9;
    g.beginPath();
    g.arc(cx + Math.cos(a) * r, cy + Math.sin(a * 1.1) * r * 0.95, 3 + (k % 3) * 1.6 + beat * 3, 0, 7);
    g.fill();
  }

  g.globalAlpha = 1;
  g.beginPath();
  const N = 96,
    B = 98;
  for (let k = 0; k <= N; k++) {
    const a = (k / N) * Math.PI * 2;
    const r =
      B *
        (1 +
          0.05 * Math.sin(3 * a + t / 700) +
          0.04 * Math.sin(5 * a - t / 520) +
          0.07 * beat * Math.sin(2 * a + t / 260)) +
      beat * 12;
    const x = cx + Math.cos(a) * r,
      y = cy + Math.sin(a) * r * 0.96;
    k ? g.lineTo(x, y) : g.moveTo(x, y);
  }
  const gr = g.createRadialGradient(cx - 34, cy - 52, 8, cx, cy, B + 26);
  gr.addColorStop(0, c1);
  gr.addColorStop(0.55, c2);
  gr.addColorStop(1, c3);
  g.fillStyle = gr;
  g.fill();

  g.fillStyle = "hsl(0 0% 100% / .35)";
  g.beginPath();
  g.ellipse(cx - 40, cy - 66, 20, 9, -0.6, 0, 7);
  g.fill();
  g.fillStyle = "hsl(345 90% 70% / .45)";
  [-1, 1].forEach((d) => {
    g.beginPath();
    g.ellipse(cx + d * 58, cy + 22, 13, 8, 0, 0, 7);
    g.fill();
  });

  const sleepy = !hasTrack ? 0.14 : playing ? 1 : 0.45;
  const blink = t % 4300 < 130 ? 0.12 : 1;
  const k = Math.min(sleepy, blink);
  [-1, 1].forEach((d) => {
    const ex = cx + d * 34,
      ey = cy - 10;
    g.fillStyle = "#fff";
    g.beginPath();
    g.ellipse(ex, ey, 21, 21 * k, 0, 0, 7);
    g.fill();
    const dx = p.x - ex,
      dy = p.y - ey,
      m = Math.hypot(dx, dy) || 1,
      o = Math.min(9, m / 14);
    g.fillStyle = "#1a1230";
    g.beginPath();
    g.ellipse(ex + (dx / m) * o, ey + (dy / m) * o * k, 10, 10 * Math.min(1, k * 1.3), 0, 0, 7);
    g.fill();
    g.fillStyle = "#fff";
    g.beginPath();
    g.arc(ex + (dx / m) * o + 3, ey + (dy / m) * o * k - 3, 2.6 * k, 0, 7);
    g.fill();
  });

  g.strokeStyle = "#1a1230";
  g.fillStyle = "#1a1230";
  g.lineWidth = 6;
  g.lineCap = "round";
  g.beginPath();
  if (!hasTrack) {
    g.moveTo(cx - 10, cy + 38);
    g.quadraticCurveTo(cx, cy + 42, cx + 10, cy + 38);
    g.stroke();
  } else if (playing) {
    const o = 8 + beat * 16;
    g.moveTo(cx - 18, cy + 30);
    g.quadraticCurveTo(cx, cy + 30 + o + 10, cx + 18, cy + 30);
    g.closePath();
    g.fill();
    g.stroke();
  } else {
    g.arc(cx, cy + 40, 6, 0, 7);
    g.stroke();
  }

  if (!playing && hasTrack) {
    g.fillStyle = "#fff";
    g.font = "700 22px Unbounded, sans-serif";
    g.globalAlpha = 0.6 + 0.4 * Math.sin(t / 500);
    g.fillText("z", cx + 96, cy - 84);
    g.font = "700 15px Unbounded, sans-serif";
    g.fillText("z", cx + 118, cy - 104);
    g.globalAlpha = 1;
  }

  if (zoomed) g.restore();
}

export function Blob({ playing, hasTrack, hue, dense = false, onToggle }: BlobProps) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const pointer = useRef<Point>({ x: 210, y: 250 });
  const beat = useRef(0);
  const zoom = dense ? DENSE_ZOOM : 1;
  // Latest props for the animation loop, which is mounted once.
  const live = useRef({ playing, hasTrack, hue, zoom });
  live.current = { playing, hasTrack, hue, zoom };

  // The pupils follow the cursor over the whole window, as in the design.
  useEffect(() => {
    const onMove = (e: PointerEvent) => {
      const el = canvas.current;
      if (!el) return;
      const b = el.getBoundingClientRect();
      if (!b.width || !b.height) return;
      pointer.current = {
        x: ((e.clientX - b.left) * SIZE) / b.width,
        y: ((e.clientY - b.top) * SIZE) / b.height,
      };
    };
    addEventListener("pointermove", onMove);
    return () => removeEventListener("pointermove", onMove);
  }, []);

  useEffect(() => {
    let raf = 0;
    let n = 0;
    const tick = (t: number) => {
      n++;
      const s = live.current;
      // Idle frames are identical: repaint those at a fifth of the rate.
      if (s.playing || n % 5 === 0)
        draw(canvas.current, t, s.playing, s.hasTrack, s.hue, pointer.current, beat, s.zoom);
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, []);

  return (
    <canvas
      className="blob"
      ref={canvas}
      width={SIZE}
      height={SIZE}
      role="button"
      tabIndex={0}
      aria-label="Mascot: click to play or pause"
      onClick={onToggle}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          // Space is also a window-level shortcut; without this the mascot
          // would toggle twice and land back where it started.
          e.stopPropagation();
          onToggle();
        }
      }}
    />
  );
}
