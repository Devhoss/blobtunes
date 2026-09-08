import { useEffect, useRef, useReducer, useState, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { isYouTubeUrl } from "./lib/youtube";
import { initialState, queueReducer, type Track } from "./lib/queue";
import { toTrack, type SearchItem } from "./lib/search";

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
  const shimRef = useRef<HTMLAudioElement>(null);
  const reqId = useRef(0);

  const current = queue.currentIndex >= 0 ? queue.items[queue.currentIndex] : undefined;
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
  const isLive = ps.playing ? ps.duration == null : current?.isLive === true;

  // subscribe to player state pushes
  useEffect(() => {
    const un = listen<PlayerState>("player://state", (e) => setPs(e.payload));
    invoke<boolean>("has_api_key").then(setHasKey);
    return () => {
      un.then((f) => f());
    };
  }, []);

  // Media Session key routing (always on — keys go to Rust, never to <audio>).
  useEffect(() => {
    if ("mediaSession" in navigator) {
      navigator.mediaSession.setActionHandler("play", () => invoke("player_play"));
      navigator.mediaSession.setActionHandler("pause", () => invoke("player_pause"));
      navigator.mediaSession.setActionHandler("previoustrack", () =>
        dispatch({ type: "prev" }),
      );
      navigator.mediaSession.setActionHandler("nexttrack", () =>
        dispatch({ type: "next" }),
      );
    }
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

  // natural end -> advance queue (user Stop never emits ended)
  useEffect(() => {
    if (!ps.ended) return;
    if (queue.items.length > 0) dispatch({ type: "next" });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ps.ended]);

  // selected track changed -> load it
  useEffect(() => {
    if (!current) return;
    invoke("player_load", { url: current.sourceUrl }).catch((e) =>
      setPs((s) => ({ ...s, error: String(e) })),
    );
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [current?.id]);

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

  const playNow = useCallback(
    (t: Track) => {
      const exists = queue.items.findIndex((x) => x.id === t.id);
      if (exists >= 0) dispatch({ type: "select", index: exists });
      else {
        dispatch({ type: "enqueue", track: t });
      }
    },
    [queue.items],
  );

  const enqueue = useCallback((t: Track) => dispatch({ type: "enqueue", track: t }), []);

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

  const toggle = () => invoke(ps.paused || !ps.playing ? "player_play" : "player_pause");
  const vol = (v: number) => {
    setPs((s) => ({ ...s, volume: v }));
    invoke("player_set_volume", { volume: v });
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
          makes the frameless window draggable; buttons opt out by default in Tauri. */}
      <div className="titlebar" data-tauri-drag-region>
        <div className="dots">
          <span />
          <span />
          <span />
        </div>
        <div className="appname">Wavesurf</div>
        <button className="gear" title="Settings" onClick={() => setShowKey((v) => !v)}>
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
        />
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
        <label className="shimrow" title="Enable only if media keys don't work without it">
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
              Results {searching && <span className="spin">…</span>}
            </h2>
            {results.map((r) => (
              <div className="item" key={r.id}>
                <img src={r.thumbnail_url} alt="" loading="lazy" />
                <div className="meta">
                  <div className="t">{r.title}</div>
                  <div className="c">
                    {r.channel}
                    {r.is_live ? " · LIVE" : r.duration ? ` · ${fmt(r.duration)}` : ""}
                  </div>
                </div>
                <div className="btns">
                  <button title="Play" onClick={() => playNow(toTrack(r))}>
                    ▶
                  </button>
                  <button title="Queue" onClick={() => enqueue(toTrack(r))}>
                    ＋
                  </button>
                </div>
              </div>
            ))}
          </section>
        )}
        <section className="queue">
          <h2>
            Queue{" "}
            {queue.items.length > 0 && (
              <button className="mini" onClick={() => dispatch({ type: "clear" })}>
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
              {t.thumbnailUrl && <img src={t.thumbnailUrl} alt="" loading="lazy" />}
              <div className="meta">
                <div className="t">{t.title}</div>
                <div className="c">
                  {t.channel} · {t.isLive ? "LIVE" : fmt(t.duration)}
                </div>
              </div>
              <button
                className="x"
                onClick={(e) => {
                  e.stopPropagation();
                  if (i === queue.currentIndex) invoke("player_stop").catch(() => {});
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
            <div className="np-art">{(current?.title ?? ps.title ?? "W").trim()[0]}</div>
          )}
          <div className="np-meta">
            <div className="np-title">{current?.title ?? ps.title ?? "Wavesurf"}</div>
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
            {isLive && ps.playing && (
              <div className="livebar">
                <i />
              </div>
            )}
            {ps.error && <div className="np-err">{ps.error.slice(0, 120)}</div>}
          </div>
        </div>
        <div className="controls">
          <button onClick={() => dispatch({ type: "prev" })}>⏮</button>
          <button className="play" onClick={toggle}>
            {ps.paused || !ps.playing ? "▶" : "⏸"}
          </button>
          <button onClick={() => dispatch({ type: "next" })}>⏭</button>
          <input
            className="vol"
            type="range"
            min={0}
            max={100}
            value={ps.volume}
            onChange={(e) => vol(Number(e.target.value))}
          />
        </div>
        {isLive || !ps.seekable || ps.duration == null ? (
          <div className="seekbar off">{isLive && ps.playing ? "live — no seeking" : ""}</div>
        ) : (
          <input
            className="seekbar"
            type="range"
            min={0}
            max={ps.duration}
            step={0.5}
            value={ps.position ?? 0}
            onChange={(e) => {
              const s = Number(e.target.value);
              setPs((p) => ({ ...p, position: s }));
              invoke("player_seek", { seconds: s });
            }}
          />
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
