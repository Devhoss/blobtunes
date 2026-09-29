import { useEffect, useRef } from "react";

/** Canvas backing size; CSS renders it 100% wide by 28px tall. */
const W = 720;
const H = 56;

export interface SquiggleSeekProps {
  /** Seconds, from the player. */
  position: number;
  /** Seconds; 0 when unknown. */
  duration: number;
  playing: boolean;
  /** False while the stream is live or not yet seekable: drawing only. */
  enabled: boolean;
  /** Live streams have no position to seek: draw a sweeping shimmer instead. */
  live?: boolean;
  hue: number;
  /** Committed once, on release — never mid-drag. */
  onSeek: (seconds: number) => void;
  /** Non-null while scrubbing, so the parent can show the pending time. */
  onScrub: (seconds: number | null) => void;
}

function stroke(
  c: HTMLCanvasElement | null,
  t: number,
  progress: number,
  playing: boolean,
  hue: number,
  ampRef: { current: number },
  live: boolean,
): void {
  if (!c) return;
  const g = c.getContext("2d");
  if (!g) return;

  // Live: no position exists, so the slot shows a gradient band sweeping the
  // full width instead of a fill up to the playhead.
  if (live) {
    const o = ((t / 5) % (W + 320)) - 160;
    g.clearRect(0, 0, W, H);
    g.lineCap = "round";
    g.lineWidth = 7;
    g.strokeStyle = "#ffffff20";
    g.beginPath();
    g.moveTo(6, H / 2);
    g.lineTo(W - 6, H / 2);
    g.stroke();
    const lg = g.createLinearGradient(o - 160, 0, o + 160, 0);
    lg.addColorStop(0, "#0000");
    lg.addColorStop(0.5, `hsl(${hue} 95% 72%)`);
    lg.addColorStop(1, "#0000");
    g.strokeStyle = lg;
    g.beginPath();
    g.moveTo(6, H / 2);
    g.lineTo(W - 6, H / 2);
    g.stroke();
    return;
  }

  const p = Math.max(0, Math.min(1, progress));
  const x1 = Math.max(6, p * W);
  ampRef.current += ((playing ? 7 : 2) - ampRef.current) * 0.1;

  g.clearRect(0, 0, W, H);
  g.lineCap = "round";
  g.lineWidth = 7;
  g.strokeStyle = "#ffffff20";
  g.beginPath();
  g.moveTo(x1, H / 2);
  g.lineTo(W - 6, H / 2);
  g.stroke();

  const gr = g.createLinearGradient(0, 0, W, 0);
  gr.addColorStop(0, `hsl(${hue} 95% 72%)`);
  gr.addColorStop(1, `hsl(${(hue + 60) % 360} 88% 58%)`);
  g.strokeStyle = gr;
  g.beginPath();
  for (let x = 6; x <= x1; x += 4) {
    const y = H / 2 + Math.sin(x / 16 - t / 220) * ampRef.current * Math.min(1, (x - 6) / 40);
    x === 6 ? g.moveTo(x, y) : g.lineTo(x, y);
  }
  g.stroke();

  g.fillStyle = "#fff";
  g.beginPath();
  g.roundRect(x1 - 5, H / 2 - 17, 10, 34, 5);
  g.fill();
}

export function SquiggleSeek({
  position,
  duration,
  playing,
  enabled,
  live = false,
  hue,
  onSeek,
  onScrub,
}: SquiggleSeekProps) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const drag = useRef(false);
  const local = useRef(0);
  const amp = useRef(2);
  const props = useRef({ position, duration, playing, enabled, hue, live });
  props.current = { position, duration, playing, enabled, hue, live };

  useEffect(() => {
    let raf = 0;
    let n = 0;
    const tick = (t: number) => {
      n++;
      const s = props.current;
      // The shimmer is motion: keep it at full rate even while paused.
      if (s.playing || s.live || n % 5 === 0) {
        const value = drag.current ? local.current : s.position;
        stroke(canvas.current, t, s.duration ? value / s.duration : 0, s.playing, s.hue, amp, s.live);
      }
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, []);

  const secondsAt = (clientX: number) => {
    const el = canvas.current;
    if (!el || !props.current.duration) return 0;
    const b = el.getBoundingClientRect();
    if (!b.width) return 0;
    const frac = Math.max(0, Math.min(1, (clientX - b.left) / b.width));
    return frac * props.current.duration;
  };

  return (
    <canvas
      ref={canvas}
      width={W}
      height={H}
      className="squiggle"
      role="slider"
      aria-label="Seek"
      aria-valuemin={0}
      aria-valuemax={Math.round(duration || 0)}
      aria-valuenow={Math.round(drag.current ? local.current : position)}
      tabIndex={enabled ? 0 : -1}
      onPointerDown={(e) => {
        if (!enabled) return;
        drag.current = true;
        e.currentTarget.setPointerCapture(e.pointerId);
        local.current = secondsAt(e.clientX);
        onScrub(local.current);
      }}
      onPointerMove={(e) => {
        if (!drag.current) return;
        local.current = secondsAt(e.clientX);
        onScrub(local.current);
      }}
      onPointerUp={() => {
        if (!drag.current) return;
        drag.current = false;
        onScrub(null);
        onSeek(local.current);
      }}
      onPointerCancel={() => {
        if (!drag.current) return;
        drag.current = false;
        onScrub(null);
      }}
      onKeyDown={(e) => {
        if (!enabled) return;
        const step = e.key === "ArrowRight" ? 5 : e.key === "ArrowLeft" ? -5 : 0;
        if (!step) return;
        e.preventDefault();
        // The window-level shortcut owns arrows too; claim them here instead.
        e.stopPropagation();
        onSeek(Math.max(0, Math.min(duration, position + step)));
      }}
    />
  );
}
