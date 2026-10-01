import { useEffect, useRef, useReducer, useState, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { PhysicalSize } from "@tauri-apps/api/dpi";
import { extractPlaylistId, isYouTubeUrl, watchUrl } from "./lib/youtube";
import {
  initialState,
  queueReducer,
  loadQueue,
  serializeQueue,
  QUEUE_KEY,
  type Track,
} from "./lib/queue";
import { toTrack, type SearchItem } from "./lib/search";
import { rowPrimaryAction, restoreVolume } from "./lib/nowPlaying";
import {
  nextRepeatMode,
  prefetchTarget,
  resolveEndedAction,
  type RepeatMode,
} from "./lib/nowPlaying";
import { hueOf } from "./lib/theme";
import { Blob } from "./components/Blob";
import { SquiggleSeek } from "./components/SquiggleSeek";

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

/** Window geometry, in CSS pixels — what the design was drawn against. 600 is
 * the design's own card height: the list scrolls inside it, and anything the
 * design has no room for (the key panel) falls back to the card's own scroll. */
const NORMAL = { w: 390, h: 600, minW: 340, minH: 520 };
const MINI = { w: 390, h: 206, minW: 340, minH: 150 };

/** The design's three views: the card itself, live search results, the queue. */
type View = "now" | "results" | "queue";

/** Size a window so the WEBVIEW gets `w`x`h` CSS pixels.
 *
 * Measured here: the display runs at 125% (window dpi 120, webview
 * devicePixelRatio 1.25), so one CSS pixel is 1.25 window pixels. setSize takes
 * PHYSICAL pixels, hence the multiply — without it the webview gets 390x700
 * physical = 312x560 CSS and the whole card renders a fifth too small.
 * Read that measurement with a DPI-AWARE probe: an unaware shell is handed the
 * client rect in virtualized units, which reads as if dpr were 1. */
function cssSize(w: number, h: number): PhysicalSize {
  const dpr = window.devicePixelRatio || 1;
  return new PhysicalSize(Math.round(w * dpr), Math.round(h * dpr));
}

const PLAY_ICON = "M3 1.5v13L14 8z";
const PAUSE_ICON = "M3 2h3.5v12H3zM9.5 2H13v12H9.5z";

const fmt = (s: number | null) => {
  if (s == null || !isFinite(s)) return "--:--";
  const m = Math.floor(s / 60),
    sec = Math.floor(s % 60);
  return `${m}:${String(sec).padStart(2, "0")}`;
};

/** Strip tile art: the real thumbnail over the track's hue, or the hue alone
 * when a track has no art yet. */
function tile(id: string, thumb?: string): React.CSSProperties {
  const h = hueOf(id);
  const gradient = `linear-gradient(135deg,hsl(${h} 90% 62%),hsl(${(h + 60) % 360} 85% 40%))`;
  return thumb
    ? { backgroundImage: `url("${thumb}"), ${gradient}` }
    : { backgroundImage: gradient };
}

export default function App() {
  // List-only resume: the saved queue comes back as a LIST — selection never
  // returns, so the app cannot startle you with sound on launch.
  const [queue, dispatch] = useReducer(queueReducer, initialState, () =>
    loadQueue(localStorage.getItem(QUEUE_KEY)),
  );
  useEffect(() => {
    try {
      localStorage.setItem(QUEUE_KEY, serializeQueue(queue));
    } catch {
      // private mode / quota: persistence is best-effort, never load-bearing
    }
  }, [queue]);
  const [ps, setPs] = useState<PlayerState>(EMPTY_STATE);
  const [query, setQuery] = useState("");
  const [results, setResults] = useState<SearchItem[] | null>(null);
  const [searching, setSearching] = useState(false);
  const [searchErr, setSearchErr] = useState("");
  const [hasKey, setHasKey] = useState(true);
  const [showKey, setShowKey] = useState(false);
  const [keyInput, setKeyInput] = useState("");
  const [mini, setMiniState] = useState(false);
  // NOW is the design's card; RESULTS and QUEUE shrink the blob into a header
  // and hand the rest of the window to the list.
  const [view, setView] = useState<View>("now");
  // Which query the current results belong to: Enter plays the top hit only
  // when the results match what is in the box.
  const [resultsQuery, setResultsQuery] = useState("");
  // Enter skips the search debounce; bumping this re-runs the search effect.
  const [forceTick, setForceTick] = useState(0);
  // The status strip's idle chatter. Loading and errors are derived, not
  // stored, so they can never be masked by a stale message.
  const [msg, setMsg] = useState("feed me a link");
  const [scrub, setScrub] = useState<number | null>(null);
  // Conditional SMTC fallback (OFF by default): a silent looping element keeps
  // the Windows media session alive IF media keys don't reach the app without
  // it. Verify shimless first; this mounts nothing unless enabled.
  const [smtcShim, setSmtcShim] = useState(
    () => localStorage.getItem("blobtunes:smtcShim") === "1",
  );
  // Repeat policy: off -> all -> one -> off, persisted like the shim.
  const [repeatMode, setRepeatMode] = useState<RepeatMode>(() => {
    const v = localStorage.getItem("blobtunes:repeat");
    return v === "all" || v === "one" ? v : "off";
  });
  function setRepeat(m: RepeatMode) {
    setRepeatMode(m);
    localStorage.setItem("blobtunes:repeat", m);
  }
  const listRef = useRef<HTMLDivElement>(null);
  /** Row index keyboard focus was on, so a re-render can put it back. */
  const focusedRow = useRef<number | null>(null);
  /** Set by Enter: search now rather than after the debounce. */
  const forceNow = useRef(false);
  /** Set by Enter: play the top hit as soon as the search lands. */
  const playTop = useRef(false);
  const shimRef = useRef<HTMLAudioElement>(null);
  const reqId = useRef(0);
  const commandBusy = useRef(false);
  const endConsumed = useRef(false); // one action per ended=true episode (see below)
  const volumeDrag = useRef<number | null>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const lastVol = useRef(80); // last non-zero volume, for unmute restore
  // Prefetch bookkeeping: the id fired for the background resolve (one
  // fire per target — position pushes are continuous) and the committed
  // pick the next shuffle advance consumes so the resolve is never wasted.
  const prefetchedId = useRef<string | null>(null);
  const committedNext = useRef<string | null>(null);

  const current =
    queue.currentIndex >= 0 ? queue.items[queue.currentIndex] : undefined;
  const hue = hueOf(current?.id);
  const sounding = ps.playing && !ps.paused;
  const trimmed = query.trim();
  const isUrl = isYouTubeUrl(trimmed);
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
  //   with reality until mpv resolves the stream — the UI MUST flip to the mpv-derived
  //   state on the first player://state push after load, even if it contradicts the hint.
  // - An unplayable track (private/deleted/region-blocked) surfaces as probe error (paste)
  //   or mpv EndFile(Error) (search) — never as a queue flag.
  const isLive = current?.isLive === true || (ps.playing && ps.duration == null);
  const canSeek = !isLive && ps.seekable && ps.duration != null;

  // The whole design is tinted from one hue custom property.
  useEffect(() => {
    document.documentElement.style.setProperty("--h", String(hue));
  }, [hue]);

  // Pin the window to the CSS pixels the design was drawn against, so the card
  // renders at its authored size on this display and mini agrees with it.
  useEffect(() => {
    void (async () => {
      try {
        const w = getCurrentWindow();
        await w.setMinSize(cssSize(NORMAL.minW, NORMAL.minH));
        await w.setSize(cssSize(NORMAL.w, NORMAL.h));
      } catch {
        /* window chrome is best-effort */
      }
    })();
  }, []);

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
      artist: current.channel || "Blobtunes",
      album: current.isLive ? "Live" : "YouTube",
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [current?.id]);

  function setShim(v: boolean) {
    setSmtcShim(v);
    localStorage.setItem("blobtunes:smtcShim", v ? "1" : "0");
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
      shuffle: queue.shuffle === true,
    });
    if (act === "reload") {
      invoke("player_load", { url: current.sourceUrl }).catch((e) =>
        setPs((s) => ({ ...s, error: String(e) })),
      );
      return;
    }
    if (queue.items.length > 0)
      dispatch(
        act === "wrap"
          ? { type: "wrap" }
          : {
              type: "next",
              freshPass: repeatMode === "all",
              roll: Math.random(),
              // The prefetched track wins the pick when still legal, so the
              // background resolve lands on the player instead of the dice
              // picking someone else and paying a fresh inline dump.
              commitId: committedNext.current ?? undefined,
            },
      );
    committedNext.current = null;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ps.ended, current?.id, queue.items.length, queue.currentIndex, queue.shuffle, repeatMode]);

  // Background prefetch of the next target. Fires in the last 45 seconds
  // of the current track (or right away for shorter ones) — the 60-min
  // resolve TTL plus single-flight in the backend mean one cheap dump buys
  // an instant advance, and a play racing the prefetch just joins it.
  // Live tracks never trigger this: their pushes carry duration=null.
  // prefetchedId is the fire-once guard; the guard self-clears when the
  // computed target changes (queue edits, shuffle toggles, skips), which
  // is exactly the invalidation the commit needs — and the reducer re-
  // validates the commit before honoring it.
  useEffect(() => {
    if (!ps.playing || ps.duration == null || ps.duration <= 0) return;
    const pos = ps.position ?? 0;
    if (ps.duration > 45 && ps.duration - pos > 45) return;
    const idx = prefetchTarget(queue, repeatMode);
    if (idx === null) return;
    const target = queue.items[idx];
    if (!target || target.id === current?.id) return;
    if (prefetchedId.current === target.id) return;
    prefetchedId.current = target.id;
    committedNext.current = target.id;
    invoke("prefetch_url", { url: target.sourceUrl }).catch(() => {
      // Silent: a failed prefetch just means a normal inline resolve later.
    });
  }, [ps.playing, ps.position, ps.duration, queue, repeatMode, current?.id]);

  // selected track changed -> load it
  useEffect(() => {
    if (!current) return;
    setMsg("sniffing this one out…");
    invoke("player_load", { url: current.sourceUrl }).catch((e) =>
      setPs((s) => ({ ...s, error: String(e) })),
    );
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [current?.id, current?.sourceUrl]);

  // debounced search (400ms; stale responses dropped by request id).
  // A pasted URL is never a search: the strip stays on the queue and the
  // GO button becomes an "add this link" action instead.
  useEffect(() => {
    const q = trimmed;
    if (isUrl || q.length < 2) {
      setResults(null);
      setResultsQuery("");
      setSearchErr("");
      playTop.current = false;
      return;
    }
    if (!hasKey) {
      setSearchErr("No YouTube API key — open Settings (⚙).");
      setResults(null);
      return;
    }
    const id = ++reqId.current;
    const immediate = forceNow.current;
    forceNow.current = false;
    setSearching(true);
    setSearchErr("");
    const t = setTimeout(async () => {
      try {
        const r = await invoke<SearchItem[]>("search_youtube", { query: q });
        if (id === reqId.current) {
          setResults(r);
          setResultsQuery(q);
          // "One Enter plays the top hit": the search is already live, so Enter
          // only has to wait for a round trip that is already in flight.
          const top = playTop.current ? r[0] : undefined;
          playTop.current = false;
          setMsg(
            r.length
              ? `${r.length} result${r.length > 1 ? "s" : ""}`
              : `no results for “${q}”`,
          );
          if (top) dispatch({ type: "play", track: toTrack(top) });
        }
      } catch (e) {
        if (id === reqId.current) {
          setSearchErr(String(e).slice(0, 180));
          setResults(null);
        }
      } finally {
        if (id === reqId.current) setSearching(false);
      }
    }, immediate ? 0 : 400);
    return () => clearTimeout(t);
  }, [trimmed, hasKey, isUrl, forceTick]);

  const playNow = useCallback((t: Track) => dispatch({ type: "play", track: t }), []);

  const enqueue = useCallback(
    (t: Track) => dispatch({ type: "enqueue", track: t }),
    [],
  );

  const enqueueAll = useCallback(
    (ts: Track[]) => dispatch({ type: "enqueue_all", tracks: ts }),
    [],
  );

  // Emptying the queue has to stop the sound too: a track the user removed must
  // not keep playing from a queue that no longer holds it.
  function clearQueue() {
    if (current) invoke("player_stop").catch(() => {});
    dispatch({ type: "clear" });
    setMsg("queue cleared");
  }

  const sendPlaybackCommand = useCallback(
    async (name: string, args?: Record<string, unknown>) => {
      if (commandBusy.current) return;
      commandBusy.current = true;
      try {
        await invoke(name, args);
      } catch (e) {
        setPs((s) => ({ ...s, error: String(e) }));
      } finally {
        commandBusy.current = false;
      }
    },
    [],
  );

  const toggle = useCallback(() => {
    setMsg(sounding ? "shh…" : "vibing");
    sendPlaybackCommand(ps.paused || !ps.playing ? "player_play" : "player_pause");
  }, [ps.paused, ps.playing, sounding, sendPlaybackCommand]);

  // Commit one seek on release. Sending a seek per drag event used to start a
  // reload whose state push unmounted the control mid-drag; the squiggle owns
  // the drag value locally and commits exactly once here.
  const commitSeek = useCallback(
    (seconds: number) => {
      setPs((p) => ({ ...p, position: seconds }));
      invoke("player_seek", { seconds }).catch((e) =>
        setPs((p) => ({ ...p, error: String(e) })),
      );
    },
    [],
  );

  const vol = (v: number) => {
    volumeDrag.current = v;
    setPs((s) => ({ ...s, volume: v }));
    localStorage.setItem("blobtunes:volume", String(v));
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
  // Volume survives restarts: the remembered value is pushed to mpv once on
  // mount (the player thread queues commands received during init, so the
  // ordering is safe).
  useEffect(() => {
    const raw = localStorage.getItem("blobtunes:volume");
    if (raw === null) return;
    const v = Math.round(Number(raw));
    if (Number.isFinite(v) && v >= 0 && v <= 100)
      invoke("player_set_volume", { volume: v }).catch(() => {});
  }, []);
  // Mute toggle: fire-and-forget, NO optimistic update. The owner loop
  // pushes state on change within a tick, and position ticks emit pushes
  // carrying the pre-command volume in between — an optimistic volume
  // visibly flaps old -> new -> old -> new (the same reason slider drags
  // use the volumeDrag guard instead of optimistic updates).
  const toggleMute = () => {
    const v = ps.volume === 0 ? restoreVolume(lastVol.current) : 0;
    localStorage.setItem("blobtunes:volume", String(v));
    invoke("player_set_volume", { volume: v }).catch((e) =>
      setPs((s) => ({ ...s, error: String(e) })),
    );
  };

  async function addLink() {
    const raw = trimmed;
    if (extractPlaylistId(raw)) {
      setSearchErr("");
      setMsg("unrolling the playlist…");
      try {
        const metas = await invoke<{
          id: string;
          title: string;
          channel: string;
          duration: number | null;
          is_live: boolean;
          thumbnail: string | null;
        }[]>("fetch_playlist", { url: raw });
        enqueueAll(
          metas.map((m) => ({
            id: m.id,
            // Per-video watch URLs: playback resolves each entry on its own,
            // and the queue stays correct if the playlist later changes.
            sourceUrl: watchUrl(m.id),
            title: m.title,
            channel: m.channel,
            duration: m.is_live ? null : m.duration,
            isLive: m.is_live,
            thumbnailUrl: m.thumbnail ?? undefined,
          })),
        );
        setQuery("");
        setResultsQuery("");
        setMsg(`added ${metas.length} to the drawer`);
        setView("queue");
      } catch (e) {
        setSearchErr(String(e).slice(0, 180));
        setMsg("nothing local · paste a link");
      }
      return;
    }
    if (!isYouTubeUrl(raw)) {
      setSearchErr("Not a valid public YouTube URL.");
      return;
    }
    setSearchErr("");
    setMsg("sniffing this one out…");
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
      setQuery("");
      setResultsQuery("");
      setMsg("added to the drawer");
      setView("queue");
    } catch (e) {
      setSearchErr(String(e).slice(0, 180));
      setMsg("nothing local · paste a link");
    }
  }

  // The list is an ordinary vertical scroller, so the wheel needs no help from
  // us (which is what retired the old non-passive wheel workaround) — but a
  // re-render rebuilds every row, dropping keyboard focus to the body and
  // stranding the ↑/↓ walk. Put it back on the same row when that happens.
  useEffect(() => {
    const k = focusedRow.current;
    if (k == null || k < 0) return;
    if (document.activeElement && document.activeElement !== document.body) return;
    listRef.current?.querySelectorAll<HTMLElement>(".row")[k]?.focus();
  });

  // Shrink the real window, not just the layout: the design's mini card is
  // ~200px tall. Best-effort — the layout still collapses if chrome refuses.
  const setMini = useCallback((on: boolean) => {
    setMiniState(on);
    void (async () => {
      try {
        const w = getCurrentWindow();
        const g = on ? MINI : NORMAL;
        await w.setMinSize(cssSize(g.minW, g.minH));
        await w.setSize(cssSize(g.w, g.h));
      } catch {
        /* window chrome is best-effort */
      }
    })();
  }, []);

  const seekBy = (delta: number) => {
    if (!canSeek) return;
    const next = Math.max(0, Math.min(ps.duration ?? 0, (ps.position ?? 0) + delta));
    commitSeek(next);
  };

  // Keyboard shortcuts. The listener is bound once and reads the latest
  // handlers through a ref, so a state tick never re-subscribes it.
  const keys = useRef({ toggle, seekBy, setMini, mini, dispatch, setView });
  keys.current = { toggle, seekBy, setMini, mini, dispatch, setView };
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const el = e.target as HTMLElement | null;
      const typing =
        !!el && (el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.isContentEditable);
      // Esc always goes back to the card, from any view — including the search
      // box, where it also gives up the caret.
      if (e.key === "Escape") {
        if (typing) el?.blur();
        keys.current.setView("now");
        return;
      }
      if (e.key === "/" && !typing) {
        e.preventDefault();
        searchRef.current?.focus();
        return;
      }
      if (typing) return;
      const k = keys.current;
      if (e.code === "Space") {
        e.preventDefault();
        k.toggle();
      } else if (e.key === "ArrowRight") k.seekBy(5);
      else if (e.key === "ArrowLeft") k.seekBy(-5);
      else if (e.key === "n") k.dispatch({ type: "next" });
      else if (e.key === "p") k.dispatch({ type: "prev" });
      else if (e.key === "m") k.setMini(!k.mini);
    };
    addEventListener("keydown", onKey);
    return () => removeEventListener("keydown", onKey);
  }, []);

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

  const err = ps.error || "";
  const status = err ? err : ps.loading ? "sniffing this one out…" : msg;
  const dense = view !== "now";

  /** ↑/↓ walk the rows; Enter/Space activate the focused one and must not fall
   * through to the window shortcuts (Space there means play/pause). */
  function rowKeys(e: React.KeyboardEvent, index: number, activate: () => void) {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      e.stopPropagation();
      activate();
      return;
    }
    const step = e.key === "ArrowDown" ? 1 : e.key === "ArrowUp" ? -1 : 0;
    if (!step) return;
    e.preventDefault();
    listRef.current?.querySelectorAll<HTMLElement>(".row")[index + step]?.focus();
  }

  return (
    <div className={`win${mini ? " mini" : ""}${dense ? " dense" : ""}`}>
      {/* Sole window chrome (decorations:false, transparent): the design's
          titlebar is the drag region. Buttons opt out automatically in Tauri. */}
      <div className="tb">
        <div className="drag" data-tauri-drag-region>
          BLOBTUNES
        </div>
        <button
          className="tb-btn"
          title="Settings"
          aria-label="Settings"
          onClick={() => setShowKey((v) => !v)}
        >
          ⚙
        </button>
        <button
          className="tb-btn"
          title={mini ? "Full player (M)" : "Mini mode (M)"}
          aria-label={mini ? "Full player" : "Mini mode"}
          onClick={() => setMini(!mini)}
        >
          {mini ? "▢" : "–"}
        </button>
        <button
          className="tb-btn"
          title="Close (hide to tray)"
          aria-label="Close"
          onClick={() => getCurrentWindow().close()}
        >
          ×
        </button>
      </div>

      <form
        className="cmd"
        onSubmit={(e) => {
          e.preventDefault();
          if (!trimmed) return;
          if (isUrl) {
            void addLink();
            return;
          }
          if (!hasKey) {
            setSearchErr("No YouTube API key — open Settings (⚙).");
            return;
          }
          // Search is already live as you type, so Enter means "play the top
          // hit": straight away when the results are current, otherwise as
          // soon as the round trip Enter just skipped the debounce for lands.
          setView("results");
          if (results && resultsQuery === trimmed && results.length > 0) {
            dispatch({ type: "play", track: toTrack(results[0]) });
            return;
          }
          playTop.current = true;
          forceNow.current = true;
          setForceTick((n) => n + 1);
        }}
      >
        <input
          id="q"
          ref={searchRef}
          autoComplete="off"
          spellCheck={false}
          placeholder="what are we listening to?"
          aria-label="Search or paste a YouTube link"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
        />
        <button type="submit" disabled={!trimmed}>
          {isUrl ? "＋" : "GO"}
        </button>
      </form>

      {searchErr && <div className="err">{searchErr}</div>}

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
          <label className="shimrow" title="Enable only if media keys don't work without it">
            <input
              type="checkbox"
              checked={smtcShim}
              onChange={(e) => setShim(e.target.checked)}
            />
            Media-key fallback (SMTC shim)
          </label>
        </div>
      )}
      {smtcShim && <audio ref={shimRef} src="silence.wav" hidden />}

      <section className="body">
        <Blob
          playing={sounding}
          hasTrack={!!current}
          hue={hue}
          dense={dense || mini}
          onToggle={toggle}
        />
        <div className="info">
          <h1 title={current?.title ?? undefined}>
            {current?.title ?? ps.title ?? "It's quiet in here"}
          </h1>
          <div className="by">
            {current
              ? current.channel || "youtube"
              : "feed me a link or a song name"}
          </div>
        </div>
        <div className="seek">
          {/* Live draws its shimmer INSIDE the same canvas (see SquiggleSeek),
              so a live<->VOD switch never reshuffles the card's layout. */}
          <SquiggleSeek
            position={ps.position ?? 0}
            duration={ps.duration ?? 0}
            playing={sounding}
            enabled={canSeek}
            live={isLive}
            hue={hue}
            onSeek={commitSeek}
            onScrub={setScrub}
          />
          <div className="times">
            <span>{fmt(scrub ?? ps.position)}</span>
            <span>
              {isLive ? (
                <span className="live">
                  <span className="dot">●</span> LIVE
                </span>
              ) : (
                fmt(ps.duration ?? current?.duration ?? null)
              )}
            </span>
          </div>
        </div>
        {/* The design's row is exactly prev/play/next, centred. The repeat coin
            is ours, so it rides absolutely at the right edge instead of
            pushing the play pill off the card's centre line. */}
        <div className="ctl">
          {/* The shuffle coin rides at the LEFT edge, mirroring the repeat
              coin on the right: two real 40px coins + gaps replace the old
              phantom padding and keep the play pill on the centre line. */}
          <button
            className={`b shuffle${queue.shuffle === true ? " on" : ""}`}
            title={queue.shuffle === true ? "Shuffle: on" : "Shuffle: off"}
            aria-label={queue.shuffle === true ? "Shuffle: on" : "Shuffle: off"}
            aria-pressed={queue.shuffle === true}
            onClick={() => {
              const next = queue.shuffle !== true;
              dispatch({ type: "toggle_shuffle" });
              setMsg(next ? "shuffling the rest" : "back in order");
            }}
          >
            <svg
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
              aria-hidden="true"
            >
              <path d="M3 6h5l9 12h3.5" />
              <path d="M3 18h5l9-12h3.5" />
              <path d="M17.5 3.5 21 6l-3.5 2.5" />
              <path d="M17.5 15.5 21 18l-3.5 2.5" />
            </svg>
          </button>
          <span className="transport">
          <button
            className="b"
            title="Previous (P)"
            aria-label="Previous"
            onClick={() => dispatch({ type: "prev" })}
          >
            <svg viewBox="0 0 16 16">
              <path d="M2 2h2v12H2zM14 2v12L5 8z" />
            </svg>
          </button>
          <button
            className="b go"
            title={sounding ? "Pause (Space)" : "Play (Space)"}
            aria-label={sounding ? "Pause" : "Play"}
            onClick={toggle}
          >
            <svg viewBox="0 0 16 16">
              <path d={sounding ? PAUSE_ICON : PLAY_ICON} />
            </svg>
          </button>
          <button
            className="b"
            title="Next (N)"
            aria-label="Next"
            onClick={() => dispatch({ type: "next" })}
          >
            <svg viewBox="0 0 16 16">
              <path d="M12 2h2v12h-2zM2 2l9 6-9 6z" />
            </svg>
          </button>
          </span>
          <button
            className={`b repeat${repeatMode !== "off" ? " on" : ""}`}
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
            <svg viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
              <path d="M7 7h10v3l4-4-4-4v3H5v6h2V7zm10 10H7v-3l-4 4 4 4v-3h12v-6h-2v4z" />
            </svg>
            {repeatMode === "one" && <span className="one">1</span>}
          </button>
        </div>
        <div className="vol">
          {/* The speaker emoji is gone — it rendered as a blurry 10px glyph next
              to the slider. The label carries the mute toggle instead, so the
              affordance survives the icon. */}
          <button
            className={`volbtn${ps.volume === 0 ? " off" : ""}`}
            title={ps.volume === 0 ? "Unmute" : "Mute"}
            aria-label={ps.volume === 0 ? "Unmute" : "Mute"}
            onClick={toggleMute}
          >
            VOL
          </button>
          <input
            type="range"
            min={0}
            max={100}
            value={ps.volume}
            aria-label="Volume"
            style={{ "--v": `${ps.volume}%` } as React.CSSProperties}
            onChange={(e) => vol(Number(e.target.value))}
            onPointerUp={finishVolumeDrag}
            onPointerCancel={finishVolumeDrag}
            onBlur={finishVolumeDrag}
          />
        </div>
      </section>

      {dense && (
        <div className="list" ref={listRef}>
          <div className="lh">
            <span>
              {view === "results"
                ? resultsQuery
                  ? `for “${resultsQuery}”`
                  : "results"
                : "UP NEXT"}
            </span>
            <span
              className={`lh-right${view === "results" && searching ? " pulse" : ""}`}
            >
              {view === "results"
                ? searching
                  ? "searching…"
                  : (results?.length ?? 0) > 0
                    ? "⏎ plays top"
                    : ""
                : (
                  <>
                    <span>click to jump</span>
                    {queue.items.length > 0 && (
                      <button className="clr" onClick={clearQueue}>
                        clear
                      </button>
                    )}
                  </>
                )}
            </span>
          </div>

          {view === "results" &&
            searching &&
            [0, 1, 2, 3, 4].map((k) => (
              <div className="sr" key={k} aria-hidden="true">
                <i />
                <span>
                  <b />
                  <b />
                </span>
              </div>
            ))}

          {view === "results" && !searching && (results?.length ?? 0) === 0 && (
            <div className="empty">
              {resultsQuery
                ? `no results for “${resultsQuery}”`
                : "search above to fill this list"}
            </div>
          )}
          {view === "queue" && queue.items.length === 0 && (
            <div className="empty">queue is empty · hit + on a result</div>
          )}

          {view === "results" &&
            !searching &&
            results?.map((r, k) => {
              const t = toTrack(r);
              const isCur = t.id === current?.id;
              // The row mirrors the dock (Pause/Resume) instead of repeating a
              // play glyph, so list and player can never disagree.
              const act = rowPrimaryAction({
                isCurrent: isCur,
                playing: ps.playing,
                paused: ps.paused,
              });
              const inQueue = queue.items.some((q) => q.id === t.id);
              return (
                <div
                  className={`row${isCur ? " cur" : ""}`}
                  key={t.id}
                  role="button"
                  tabIndex={k === 0 ? 0 : -1}
                  aria-label={`${act.label}: ${t.title}`}
                  title={`${t.title}${t.channel ? ` — ${t.channel}` : ""}`}
                  onFocus={() => (focusedRow.current = k)}
                  onClick={() => (act.mode === "toggle" ? toggle() : playNow(t))}
                  onKeyDown={(e) =>
                    rowKeys(e, k, () =>
                      act.mode === "toggle" ? toggle() : playNow(t),
                    )
                  }
                >
                  <div className="t" style={tile(t.id, t.thumbnailUrl)} />
                  <div className="tx">
                    <span className="n">{t.title}</span>
                    <span className="m">
                      {t.channel}
                      {r.is_live ? (
                        <span className="lv">{t.channel ? " · " : ""}● LIVE</span>
                      ) : t.duration != null ? (
                        ` · ${fmt(t.duration)}`
                      ) : (
                        ""
                      )}
                    </span>
                  </div>
                  <div className="act">
                    {k === 0 && <span className="kb">⏎</span>}
                    <button
                      className="ic"
                      title={inQueue ? "In queue" : "Add to queue"}
                      aria-label={
                        inQueue
                          ? `${t.title} is already queued`
                          : `Add ${t.title} to the queue`
                      }
                      onClick={(e) => {
                        e.stopPropagation();
                        if (inQueue) return;
                        enqueue(t);
                        setMsg("added to queue");
                      }}
                    >
                      {inQueue ? "✓" : "+"}
                    </button>
                  </div>
                </div>
              );
            })}

          {view === "queue" &&
            queue.items.map((t, k) => (
              <div
                className={`row${k === queue.currentIndex ? " cur" : ""}${
                  queue.shuffle === true && queue.played?.includes(t.id) ? " played" : ""
                }`}
                key={t.id}
                role="button"
                tabIndex={k === 0 ? 0 : -1}
                aria-label={`Play ${t.title}`}
                title={`${t.title}${t.channel ? ` — ${t.channel}` : ""}`}
                onFocus={() => (focusedRow.current = k)}
                onClick={() => dispatch({ type: "select", index: k })}
                onKeyDown={(e) =>
                  rowKeys(e, k, () => dispatch({ type: "select", index: k }))
                }
              >
                <div className="t" style={tile(t.id, t.thumbnailUrl)} />
                <div className="tx">
                  <span className="n">{t.title}</span>
                  <span className="m">
                    {t.channel}
                    {t.isLive ? (
                      <span className="lv">{t.channel ? " · " : ""}● LIVE</span>
                    ) : (
                      ` · ${fmt(t.duration)}`
                    )}
                  </span>
                </div>
                <div className="act">
                  <button
                    className="ic"
                    title="Remove"
                    aria-label={`Remove ${t.title} from the queue`}
                    onClick={(e) => {
                      e.stopPropagation();
                      if (k === queue.currentIndex)
                        invoke("player_stop").catch(() => {});
                      dispatch({ type: "remove", index: k });
                    }}
                  >
                    ×
                  </button>
                </div>
              </div>
            ))}
        </div>
      )}

      <div
        className="tabs"
        role="group"
        aria-label="View"
        style={
          {
            "--ti": view === "now" ? 0 : view === "results" ? 1 : 2,
          } as React.CSSProperties
        }
      >
        <span className="pill" aria-hidden="true" />
        <button aria-pressed={view === "now"} onClick={() => setView("now")}>
          NOW
        </button>
        <button
          aria-pressed={view === "results"}
          onClick={() => setView("results")}
        >
          RESULTS
          <b className={searching ? "dim" : undefined}>{results?.length ?? 0}</b>
        </button>
        <button aria-pressed={view === "queue"} onClick={() => setView("queue")}>
          QUEUE
          <b>{queue.items.length}</b>
        </button>
      </div>

      <div className="sb">
        <span className={`m${err ? " bad" : ""}`} title={status}>
          {status}
        </span>
        <span>SPACE · ←→ · N/P · M</span>
      </div>
    </div>
  );
}
