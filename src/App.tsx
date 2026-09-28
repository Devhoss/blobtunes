import { useEffect, useRef, useReducer, useState, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { isYouTubeUrl } from "./lib/youtube";
import { initialState, queueReducer, type Track } from "./lib/queue";
import { toTrack, type SearchItem } from "./lib/search";
import { rowPrimaryAction, restoreVolume } from "./lib/nowPlaying";
import {
  nextRepeatMode,
  resolveEndedAction,
  type RepeatMode,
} from "./lib/nowPlaying";
import { LiquidPlayGlyph } from "./components/LiquidPlay";

interface PlayerState {
  playing: boolean;
  paused: boolean;
  loading: boolean;
  position: number | null;
  duration: number | null;
  seekable: boolean;
  volume: number;
  title: string | null;
  ended: boolean;
  error: string | null;
}

const EMPTY_STATE: PlayerState = {
  playing: false,
  paused: false,
  loading: false,
  position: null,
  duration: null,
  seekable: false,
  volume: 80,
  title: null,
  ended: false,
  error: null,
};

const fmt = (s: number | null) => {
  if (s == null || !isFinite(s)) return "--:--";
  const m = Math.floor(s / 60),
    sec = Math.floor(s % 60);
  return `${m}:${String(sec).padStart(2, "0")}`;
};

export default function App() {
  const [queue, dispatch] = useReducer(queueReducer, initialState);
  const [ps, setPs] = useState<PlayerState>(EMPTY_STATE);
  const [query, setQuery] = useState("");
  const [results, setResults] = useState<SearchItem[] | null>(null);
  const [searching, setSearching] = useState(false);
  const [searchErr, setSearchErr] = useState("");
  const [url, setUrl] = useState("");
  const [hasKey, setHasKey] = useState(true);
  const [showKey, setShowKey] = useState(false);
  const [keyInput, setKeyInput] = useState("");
  // Conditional SMTC fallback (OFF by default): a silent looping element keeps
  // the Windows media session alive IF media keys don't reach the app without
  // it. Verify shimless first; this mounts nothing unless enabled.
  const [smtcShim, setSmtcShim] = useState(
    () => localStorage.getItem("wavesurf:smtcShim") === "1",
  );
  // Repeat policy: off -> all -> one -> off, persisted like the shim.
  const [repeatMode, setRepeatMode] = useState<RepeatMode>(() => {
    const v = localStorage.getItem("wavesurf:repeat");
    return v === "all" || v === "one" ? v : "off";
  });
  function setRepeat(m: RepeatMode) {
    setRepeatMode(m);
    localStorage.setItem("wavesurf:repeat", m);
  }
  const shimRef = useRef<HTMLAudioElement>(null);
  const reqId = useRef(0);
  const commandBusy = useRef(false);
  const endConsumed = useRef(false); // one action per ended=true episode (see below)
  const volumeDrag = useRef<number | null>(null);
  // Seekbar scrub state: ref is the commit-time value, state mirrors it for
  // rendering. While non-null the thumb is locally controlled and NO seek is
  // sent (see commitSeek + the seekbar JSX for why).
  const seekDragRef = useRef<number | null>(null);
  const [seekDrag, setSeekDrag] = useState<number | null>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const lastVol = useRef(80); // last non-zero volume, for unmute restore

  const current =
    queue.currentIndex >= 0 ? queue.items[queue.currentIndex] : undefined;
  // ACTIVE-TRACK LIVE-STATE AUTHORITY (explicit, binds UI + backend):
  // - `track.isLive` (from search snippet or yt-dlp probe) is a DISPLAY HINT for rows
  //   that are NOT currently playing. It is never the authority for the active track.
  // - While a track is the active selection AND the player reports it is loaded/playing
  //   (ps.playing true after FileLoaded), liveness is derived SOLELY from mpv state:
  //   live ⟺ ps.duration == null (mpv reports 0/unknown duration for live/HLS).
  // - `seekable` corroborates but does not decide: some VODs report seekable=false briefly
  //   while buffering; duration==null is the single deciding signal.
  // - Pre-play, `probe_url` is authoritative for pasted URLs (probe.is_live fills the hint).
  // - Search results play UNPROBED in MVP: a stale `liveBroadcastContent` hint may disagree
  //   with reality until mpv resolves the stream — the dock MUST flip to the mpv-derived
  //   state on the first player://state push after load, even if it contradicts the hint.
  // - An unplayable track (private/deleted/region-blocked) surfaces as probe error (paste)
  //   or mpv EndFile(Error) (search) — never as a queue flag.
  const isLive = current?.isLive === true || (ps.playing && ps.duration == null);

  // subscribe to player state pushes
  useEffect(() => {
    const un = listen<PlayerState>("player://state", (e) =>
      setPs(() => ({
        ...e.payload,
        volume: volumeDrag.current ?? e.payload.volume,
      })),
    );
    invoke<boolean>("has_api_key").then(setHasKey);
    return () => {
      un.then((f) => f());
    };
  }, []);

  // Media Session key routing (always on — keys go to Rust, never to <audio>).
  useEffect(() => {
    if ("mediaSession" in navigator) {
      navigator.mediaSession.setActionHandler("play", () =>
        invoke("player_play"),
      );
      navigator.mediaSession.setActionHandler("pause", () =>
        invoke("player_pause"),
      );
      navigator.mediaSession.setActionHandler("previoustrack", () =>
        dispatch({ type: "prev" }),
      );
      navigator.mediaSession.setActionHandler("nexttrack", () =>
        dispatch({ type: "next" }),
      );
    }
  }, []);

  // Native Windows media session (SMTC) — metadata half. The Rust player owns
  // playback state, position and the timeline; the queue owns title/channel/
  // artwork and is the only side that knows whether ⏮/⏭ have a target. Pushed
  // only when the selection or the queue geometry changes: never per tick.
  // Without it the OS widget (and Venu) would show an unnamed session while
  // yt-dlp resolves, and would offer skips that do not exist.
  useEffect(() => {
    invoke("player_smtc_meta", {
      hint: {
        title: current?.title ?? null,
        channel: current?.channel ?? null,
        artwork: current?.thumbnailUrl ?? null,
        is_live: current?.isLive === true,
        can_next:
          queue.currentIndex >= 0 && queue.currentIndex < queue.items.length - 1,
        can_prev: queue.currentIndex > 0,
      },
    }).catch(() => {}); // display-only: never surface as a playback error
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [current?.id, queue.items.length, queue.currentIndex]);

  // OS transport commands that belong to the queue (hardware media keys, the
  // Windows media flyout, Venu). They re-enter through the SAME reducer actions
  // the ⏮/⏭ buttons dispatch — no second control path, no duplicated skipping.
  useEffect(() => {
    const un = listen<string>("player://media-command", (e) => {
      if (e.payload === "next") dispatch({ type: "next" });
      else if (e.payload === "prev") dispatch({ type: "prev" });
    });
    return () => {
      un.then((f) => f());
    };
  }, []);


  // Conditional SMTC fallback element: mounted ONLY when enabled above.
  useEffect(() => {
    if (!smtcShim) return;
    const el = shimRef.current;
    if (!el) return;
    el.volume = 0.01;
    el.loop = true;
    el.play().catch(() => {});
    const once = () => {
      el.play().catch(() => {});
    };
    window.addEventListener("pointerdown", once, { once: true });
    return () => {
      window.removeEventListener("pointerdown", once);
      el.pause();
    };
  }, [smtcShim]);

  // SMTC metadata follows the current track.
  useEffect(() => {
    if (!current || !("mediaSession" in navigator)) return;
    navigator.mediaSession.metadata = new MediaMetadata({
      title: current.title,
      artist: current.channel || "Wavesurf",
      album: current.isLive ? "Live" : "YouTube",
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [current?.id]);

  function setShim(v: boolean) {
    setSmtcShim(v);
    localStorage.setItem("wavesurf:smtcShim", v ? "1" : "0");
  }

  // natural end -> repeat policy, else advance (user Stop never emits ended).
  // Live tracks always take the plain next path: never reload/wrap a live feed.
  // Exactly ONE action per ended=true episode: the flag clears asynchronously
  // (the new Load pushes ended=false), so without this latch the effect
  // re-fires on its OWN dispatches — every advance changes deps while ended
  // is still true, machine-gunning Loads until a push lands (seen in the log:
  // 14 Loads cycling 4 tracks in 2ms, landing "randomly"). The old track-id
  // guard couldn't stop it because every iteration has a new id.
  useEffect(() => {
    if (!ps.ended) {
      endConsumed.current = false;
      return;
    }
    if (endConsumed.current) return;
    const id = current?.id ?? null;
    if (!id || !current) return;
    endConsumed.current = true;
    const act = resolveEndedAction({
      mode: repeatMode,
      currentIndex: queue.currentIndex,
      length: queue.items.length,
      isLive: current.isLive === true,
    });
    if (act === "reload") {
      invoke("player_load", { url: current.sourceUrl }).catch((e) =>
        setPs((s) => ({ ...s, error: String(e) })),
      );
      return;
    }
    if (queue.items.length > 0)
      dispatch(act === "wrap" ? { type: "select", index: 0 } : { type: "next" });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ps.ended, current?.id, queue.items.length, queue.currentIndex, repeatMode]);

  // selected track changed -> load it
  useEffect(() => {
    if (!current) return;
    invoke("player_load", { url: current.sourceUrl }).catch((e) =>
      setPs((s) => ({ ...s, error: String(e) })),
    );
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [current?.id, current?.sourceUrl]);

  // debounced search (400ms; stale responses dropped by request id)
  useEffect(() => {
    const q = query.trim();
    if (q.length < 2) {
      setResults(null);
      setSearchErr("");
      return;
    }
    if (!hasKey) {
      setSearchErr("No YouTube API key — open Settings.");
      setResults(null);
      return;
    }
    const id = ++reqId.current;
    setSearching(true);
    setSearchErr("");
    const t = setTimeout(async () => {
      try {
        const r = await invoke<SearchItem[]>("search_youtube", { query: q });
        if (id === reqId.current) setResults(r);
      } catch (e) {
        if (id === reqId.current) {
          setSearchErr(String(e).slice(0, 180));
          setResults(null);
        }
      } finally {
        if (id === reqId.current) setSearching(false);
      }
    }, 400);
    return () => clearTimeout(t);
  }, [query, hasKey]);

  const playNow = useCallback((t: Track) => dispatch({ type: "play", track: t }), []);

  const enqueue = useCallback(
    (t: Track) => dispatch({ type: "enqueue", track: t }),
    [],
  );

  async function addPastedUrl() {
    const raw = url.trim();
    if (!isYouTubeUrl(raw)) {
      setSearchErr("Not a valid public YouTube URL.");
      return;
    }
    setSearchErr("");
    try {
      const meta = await invoke<{
        id: string;
        title: string;
        channel: string;
        duration: number | null;
        is_live: boolean;
        thumbnail: string | null;
      }>("probe_url", { url: raw });
      enqueue({
        id: meta.id,
        sourceUrl: raw,
        title: meta.title,
        channel: meta.channel,
        duration: meta.is_live ? null : meta.duration,
        isLive: meta.is_live,
        thumbnailUrl: meta.thumbnail ?? undefined,
      });
      setUrl("");
    } catch (e) {
      setSearchErr(String(e).slice(0, 180));
    }
  }

  const sendPlaybackCommand = useCallback(async (name: string, args?: Record<string, unknown>) => {
    if (commandBusy.current) return;
    commandBusy.current = true;
    try {
      await invoke(name, args);
    } catch (e) {
      setPs((s) => ({ ...s, error: String(e) }));
    } finally {
      commandBusy.current = false;
    }
  }, []);

  const toggle = () =>
    sendPlaybackCommand(ps.paused || !ps.playing ? "player_play" : "player_pause");
  // Commit one seek on scrub release. The old seekbar invoked player_seek on
  // EVERY change event: a post-EOF drag's first invoke started the reload,
  // whose state push (duration=null, playing=true) flipped the footer into
  // the live/off branch and UNMOUNTED the input mid-drag — the "slider
  // releases itself" bug — while the surviving first event carried the
  // stale end-of-track value. Scrub locally, commit once, on release.
  const commitSeek = () => {
    const s = seekDragRef.current;
    seekDragRef.current = null;
    setSeekDrag(null);
    if (s != null) {
      setPs((p) => ({ ...p, position: s }));
      invoke("player_seek", { seconds: s }).catch((e) =>
        setPs((p) => ({ ...p, error: String(e) })),
      );
    }
  };
  const vol = (v: number) => {
    volumeDrag.current = v;
    setPs((s) => ({ ...s, volume: v }));
    invoke("player_set_volume", { volume: v }).catch((e) =>
      setPs((s) => ({ ...s, error: String(e) })),
    );
  };
  const finishVolumeDrag = () => {
    volumeDrag.current = null;
  };
  // Track the last sounding volume so unmute restores it.
  useEffect(() => {
    if (ps.volume > 0) lastVol.current = ps.volume;
  }, [ps.volume]);
  // Mute toggle: fire-and-forget, NO optimistic update. The owner loop
  // pushes state on change within a tick, and position ticks emit pushes
  // carrying the pre-command volume in between — an optimistic volume
  // visibly flaps old -> new -> old -> new (the same reason slider drags
  // use the volumeDrag guard instead of optimistic updates).
  const toggleMute = () => {
    const v = ps.volume === 0 ? restoreVolume(lastVol.current) : 0;
    invoke("player_set_volume", { volume: v }).catch((e) =>
      setPs((s) => ({ ...s, error: String(e) })),
    );
  };

  async function saveKey() {
    try {
      await invoke("set_api_key", { key: keyInput.trim() });
      setHasKey(true);
      setShowKey(false);
      setSearchErr("");
    } catch (e) {
      setSearchErr(String(e));
    }
  }

  return (
    <div className="app">
      {/* V2 TitleBar — sole window chrome (decorations:false). data-tauri-drag-region
          makes the frameless window draggable; buttons opt out by default in Tauri.
          The attribute is repeated on dots/appname so the drag area has no
          dead spots (text nodes included). Needs
          core:window:allow-start-dragging in capabilities. */}
      <div className="titlebar" data-tauri-drag-region>
        {/* Window controls, macOS traffic-light order: close (hides to tray
            via intercept_close, same as window ✕) / minimize /
            maximize-toggle. Needs the matching core:window permissions. */}
        <div className="dots no-drag" data-tauri-drag-region="false">
          <span
            role="button"
            title="Close (hide to tray)"
            onClick={() => getCurrentWindow().close()}
          />
          <span
            role="button"
            title="Minimize"
            onClick={() => getCurrentWindow().minimize()}
          />
          <span
            role="button"
            title="Maximize"
            onClick={() => getCurrentWindow().toggleMaximize()}
          />
        </div>
        <div className="appname" data-tauri-drag-region>
          Wavesurf
        </div>
        <button
          className="gear"
          title="Settings"
          onClick={() => setShowKey((v) => !v)}
        >
          ⚙
        </button>
      </div>
      <div className="row top">
        <input
          className="search"
          placeholder="Search YouTube…"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          spellCheck={false}
          ref={searchRef}
        />
        {query && (
          <button
            className="clear"
            title="Clear search"
            aria-label="Clear search"
            onClick={() => {
              setQuery("");
              searchRef.current?.focus();
            }}
          >
            ×
          </button>
        )}
      </div>

      {showKey && (
        <div className="keypanel">
          <input
            placeholder="Paste YouTube Data API v3 key"
            value={keyInput}
            onChange={(e) => setKeyInput(e.target.value)}
            type="password"
          />
          <button onClick={saveKey}>Save</button>
          <a
            href="https://console.cloud.google.com/apis/library/youtube.googleapis.com"
            target="_blank"
            rel="noreferrer"
          >
            get a key ↗
          </a>
        </div>
      )}
      {showKey && (
        <label
          className="shimrow"
          title="Enable only if media keys don't work without it"
        >
          <input
            type="checkbox"
            checked={smtcShim}
            onChange={(e) => setShim(e.target.checked)}
          />
          Media-key fallback (SMTC shim)
        </label>
      )}
      {smtcShim && <audio ref={shimRef} src="silence.wav" hidden />}

      {searchErr && <div className="error">{searchErr}</div>}

      <div className="lists">
        {results && (
          <section className="results">
            <h2>
              Results · {results.length}{" "}
              {searching && <span className="spin">…</span>}
            </h2>
            {results.map((r) => {
              // Active row = the current queue selection: its primary button
              // mirrors the dock (Pause/Resume via toggle) instead of
              // repeating ▶, so list and player can never disagree.
              const isCur = r.id === current?.id;
              const act = rowPrimaryAction({
                isCurrent: isCur,
                playing: ps.playing,
                paused: ps.paused,
              });
              return (
              <div className={`item ${isCur ? "on" : ""}`} key={r.id}>
                <img src={r.thumbnail_url} alt="" loading="lazy" />
                <div className="meta">
                  <div className="t" title={r.title}>
                    {r.title}
                  </div>
                  <div className="c">
                    {r.channel}
                    {r.is_live
                      ? " · LIVE"
                      : r.duration
                        ? ` · ${fmt(r.duration)}`
                        : ""}
                    {isCur && <span className="np-tag"> · Now</span>}
                  </div>
                </div>
                <div className="btns">
                  <button
                    title={act.label}
                    aria-label={`${act.label}: ${r.title}`}
                    onClick={() =>
                      act.mode === "toggle" ? toggle() : playNow(toTrack(r))
                    }
                  >
                    {act.glyph}
                  </button>
                  <button title="Queue" onClick={() => enqueue(toTrack(r))}>
                    ＋
                  </button>
                </div>
              </div>
              );
            })}
          </section>
        )}
        <section className="queue">
          <h2>
            Queue{" "}
            {queue.items.length > 0 && (
              <button
                className="mini"
                onClick={() => dispatch({ type: "clear" })}
              >
                clear
              </button>
            )}
          </h2>
          {queue.items.map((t, i) => (
            <div
              className={`item q ${i === queue.currentIndex ? "on" : ""}`}
              key={t.id}
              onClick={() => dispatch({ type: "select", index: i })}
            >
              {t.thumbnailUrl && (
                <img src={t.thumbnailUrl} alt="" loading="lazy" />
              )}
              <div className="meta">
                <div className="t" title={t.title}>
                  {t.title}
                </div>
                <div className="c">
                  {t.channel} · {t.isLive ? "LIVE" : fmt(t.duration)}
                  {i === queue.currentIndex && <span className="np-tag"> · Now</span>}
                </div>
              </div>
              <button
                className="x"
                title="Remove from queue"
                aria-label={`Remove ${t.title} from queue`}
                onClick={(e) => {
                  e.stopPropagation();
                  if (i === queue.currentIndex)
                    invoke("player_stop").catch(() => {});
                  dispatch({ type: "remove", index: i });
                }}
              >
                ×
              </button>
            </div>
          ))}
        </section>
      </div>

      <footer className="now">
        <div className="np">
          {current?.thumbnailUrl ? (
            <img className="np-art" src={current.thumbnailUrl} alt="" />
          ) : (
            <div className="np-art">
              {(current?.title ?? ps.title ?? "W").trim()[0]}
            </div>
          )}
          <div className="np-meta">
            <div className="np-title">
              {current?.title ?? ps.title ?? "Wavesurf"}
            </div>
            {(current?.channel || ps.title) && (
              <div className="np-channel">{current?.channel}</div>
            )}
            <div className="np-sub">
              {ps.loading ? (
                "loading…"
              ) : isLive && ps.playing ? (
                <span className="live">
                  <span className="dot">●</span> LIVE
                </span>
              ) : current ? (
                `${fmt(ps.position)} / ${fmt(ps.duration ?? current.duration)}`
              ) : (
                "paste a YouTube URL below"
              )}
            </div>
            {ps.error && <div className="np-err">{ps.error.slice(0, 120)}</div>}
          </div>
        </div>
        <div className="controls">
          <div className="transport">
            <button
              className="step"
              onClick={() => dispatch({ type: "prev" })}
              aria-label="Previous"
            >
              ⏮
            </button>
            <button
              className="play wide"
              onClick={toggle}
              aria-label={ps.paused || !ps.playing ? "Play" : "Pause"}
            >
              <LiquidPlayGlyph
                glyph={ps.paused || !ps.playing ? "▶" : "⏸"}
                repetition={6}
              />
            </button>
            <button
              className="step"
              onClick={() => dispatch({ type: "next" })}
              aria-label="Next"
            >
              ⏭
            </button>
            <button
              className={`step repeat${repeatMode !== "off" ? " active" : ""}`}
              title={
                repeatMode === "off"
                  ? "Repeat: off"
                  : repeatMode === "all"
                    ? "Repeat queue"
                    : "Repeat one"
              }
              aria-label={
                repeatMode === "off"
                  ? "Repeat: off"
                  : repeatMode === "all"
                    ? "Repeat queue"
                    : "Repeat one"
              }
              onClick={() => setRepeat(nextRepeatMode(repeatMode))}
            >
              <svg
                viewBox="0 0 24 24"
                width="16"
                height="16"
                fill="currentColor"
                aria-hidden="true"
              >
                <path d="M7 7h10v3l4-4-4-4v3H5v6h2V7zm10 10H7v-3l-4 4 4 4v-3h12v-6h-2v4z" />
              </svg>
              {repeatMode === "one" && <span className="one-badge">1</span>}
            </button>
          </div>
          <div className="volgroup">
            <button
              className="volbtn"
              title={ps.volume === 0 ? "Unmute" : "Mute"}
              aria-label={ps.volume === 0 ? "Unmute" : "Mute"}
              onClick={toggleMute}
            >
              {ps.volume === 0 ? "🔇" : "🔊"}
            </button>
            <input
              className="vol"
              type="range"
              min={0}
              max={100}
              value={ps.volume}
              onChange={(e) => vol(Number(e.target.value))}
              onPointerUp={finishVolumeDrag}
              onPointerCancel={finishVolumeDrag}
              onBlur={finishVolumeDrag}
            />
          </div>
        </div>
        {/* While scrubbing, ALWAYS render the input (locally valued): a
            mid-drag swap to the live/off branch cancels pointer capture and
            "releases" the thumb. Scrubbing sends no commands, so no reload can
            start under the drag; the single committed seek happens on release. */}
        {isLive && ps.playing && seekDrag == null ? (
          // Live indicator lives in the seekbar slot: same footprint as VOD
          // progress, so live<->VOD switches never reshuffle the footer.
          <div className="liveslot" aria-label="Live stream">
            <div className="livebar">
              <i />
            </div>
          </div>
        ) : seekDrag != null || (!isLive && ps.seekable && ps.duration != null) ? (
          <input
            className="seekbar"
            type="range"
            min={0}
            max={ps.duration ?? 0}
            step={0.5}
            value={seekDrag ?? (ps.position ?? 0)}
            onPointerDown={() => {
              seekDragRef.current = ps.position ?? 0;
              setSeekDrag(ps.position ?? 0);
            }}
            onChange={(e) => {
              const s = Number(e.target.value);
              if (seekDragRef.current != null) {
                // Scrub: local only, committed by onPointerUp.
                seekDragRef.current = s;
                setSeekDrag(s);
              } else {
                // Keyboard/no-pointer tweak: seek immediately.
                setPs((p) => ({ ...p, position: s }));
                invoke("player_seek", { seconds: s }).catch((e) =>
                  setPs((p) => ({ ...p, error: String(e) })),
                );
              }
            }}
            onPointerUp={commitSeek}
            onPointerCancel={commitSeek}
            onBlur={commitSeek}
          />
        ) : (
          <div className="seekbar off" />
        )}
        <form
          className="row paste"
          onSubmit={(e) => {
            e.preventDefault();
            addPastedUrl();
          }}
        >
          <input
            placeholder="🔗 Paste YouTube URL…"
            value={url}
            spellCheck={false}
            onChange={(e) => setUrl(e.target.value)}
          />
          <button type="submit">＋</button>
        </form>
      </footer>
    </div>
  );
}
