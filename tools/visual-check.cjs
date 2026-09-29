/* Visual-parity harness for the Blobtunes design port.
 *
 * The design artifact is the spec and is never modified. This script renders
 * it and the running app in the same Chromium build and compares the computed
 * style of every element the design defines, so a drift shows up as a diff
 * line instead of an opinion.
 *
 * The design's views are compared like with like: the artifact's NOW view
 * against the app's NOW view, the artifact's RESULTS list against the app's.
 * Every element only one view defines reads as "-" on the other side, so a
 * view mismatch can never masquerade as a style difference.
 *
 *   npm run dev                                  # app on :1420
 *   node tools/visual-check.cjs                  # compare + write screenshots
 *
 * Playwright lives in its own checkout rather than this repo's dependencies.
 *   PLAYWRIGHT_HOME  dir holding node_modules/playwright   (default E:/dev/playwright)
 *   BLOBTUNES_BASELINE  the design artifact html           (default E:/comp/...)
 *   BLOBTUNES_APP       the running app url                (default http://localhost:1420/)
 *   BLOBTUNES_SHOTS     screenshot output dir              (default tools/shots)
 */
const fs = require("node:fs");
const path = require("node:path");
const { pathToFileURL } = require("node:url");

const PLAYWRIGHT_HOME = process.env.PLAYWRIGHT_HOME || "E:/dev/playwright";
const APP = process.env.BLOBTUNES_APP || "http://localhost:1420/";
const BASELINE =
  process.env.BLOBTUNES_BASELINE ||
  "E:/comp/Blobtunes — compact YouTube player (2).html";
const SHOTS = process.env.BLOBTUNES_SHOTS || path.join(__dirname, "shots");

// The artifact floats its 390x600 card inside a 16px body gutter.
const ARTIFACT_VIEWPORT = { width: 390 + 32, height: 600 + 32 };
// Matches NORMAL in src/App.tsx: the window's logical (and therefore CSS) size.
const APP_VIEWPORT = { width: 390, height: 600 };
// Keep in step with NORMAL/MINI in src/App.tsx.
const MINI_SIZE = { w: 390, h: 206 };

/** Everything the app reads from the Tauri backend, answered locally so the
 * real frontend can be driven in a plain browser. */
function stub() {
  const callbacks = new Map();
  const listeners = new Map();
  let nextId = 1;
  const SEARCH = [
    {
      id: "dQw4w9WgXcQ",
      title: "Never Gonna Give You Up",
      channel: "Rick Astley",
      thumbnail_url: "https://i.ytimg.com/vi/dQw4w9WgXcQ/mqdefault.jpg",
      duration: 213,
      is_live: false,
    },
    {
      id: "jfKfPfyJRdk",
      title: "lofi hip hop radio — beats to relax/study to",
      channel: "Lofi Girl",
      // No art: also exercises the hue-gradient tile fallback.
      thumbnail_url: "",
      duration: null,
      is_live: true,
    },
    {
      id: "fJ9rUzIMcZQ",
      title: "Bohemian Rhapsody (Remastered 2011)",
      channel: "Queen",
      thumbnail_url: "",
      duration: 355,
      is_live: false,
    },
    // Enough hits to overflow the list, so list scrolling is testable.
    { id: "9bZkp7q19f0", title: "Gangnam Style", channel: "PSY", thumbnail_url: "", duration: 253, is_live: false },
    { id: "kJQP7kiw5Fk", title: "Despacito", channel: "Luis Fonsi", thumbnail_url: "", duration: 282, is_live: false },
    { id: "jNQXAC9IVRw", title: "Me at the zoo", channel: "jawed", thumbnail_url: "", duration: 19, is_live: false },
    { id: "dQw4w9WgXcR", title: "A Midnight Study Session (Full Album)", channel: "Study Beats", thumbnail_url: "", duration: 3600, is_live: false },
  ];
  window.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: () => {} };
  window.__TAURI_INTERNALS__ = {
    metadata: {
      currentWindow: { label: "main" },
      currentWebview: { label: "main" },
    },
    plugins: { path: { sep: "\\", delimiter: ";" } },
    convertFileSrc: (p) => p,
    transformCallback: (cb) => {
      const id = nextId++;
      callbacks.set(id, cb);
      return id;
    },
    unregisterCallback: (id) => callbacks.delete(id),
    invoke: async (cmd, args) => {
      if (cmd === "plugin:event|listen") {
        listeners.set(args.handler, args.event);
        return nextId++;
      }
      if (cmd === "plugin:event|unlisten") return null;
      if (cmd === "has_api_key") return true;
      // The real search is a Rust round trip; the delay makes the design's
      // searching state (shimmer rows, pulsing badge) observable.
      if (cmd === "search_youtube") {
        await new Promise((r) => setTimeout(r, 650));
        return SEARCH;
      }
      if (cmd === "probe_url")
        return {
          id: "dQw4w9WgXcQ",
          title: "Never Gonna Give You Up",
          channel: "Rick Astley",
          duration: 213,
          is_live: false,
          thumbnail: "https://i.ytimg.com/vi/dQw4w9WgXcQ/mqdefault.jpg",
        };
      return null;
    },
  };
  // Push a backend event (player://state, player://media-command) from the test.
  window.__emit = (event, payload) => {
    for (const [handlerId, name] of listeners) {
      if (name !== event) continue;
      const cb = callbacks.get(handlerId);
      if (cb) cb({ event, id: handlerId, payload });
    }
  };
}

/** Computed style + geometry for every element the design pins down.
 * An element the current view does not render reads as null, and two nulls
 * compare equal, so "not in this view" is never reported as a diff. */
function probe() {
  const q = (s) => document.querySelector(s);
  const px = (el, prop) =>
    el ? getComputedStyle(el).getPropertyValue(prop).trim() : null;
  const box = (el) => {
    if (!el) return null;
    const r = el.getBoundingClientRect();
    if (!r.width && !r.height) return null; // display:none stands in for absent
    return `${Math.round(r.width)}x${Math.round(r.height)}`;
  };
  const root = getComputedStyle(document.documentElement);
  return {
    "hue var": root.getPropertyValue("--h").trim(),
    "body font": px(document.body, "font-family"),
    "body bg": px(document.body, "background-color"),
    accent: root.getPropertyValue("--a").trim(),
    accent2: root.getPropertyValue("--a2").trim(),
    "card radius": px(q(".win"), "border-radius"),
    "card border": px(q(".win"), "border-color"),
    "drag label font": px(q(".tb .drag"), "font"),
    "cmd radius": px(q(".cmd"), "border-radius"),
    "cmd box": box(q(".cmd")),
    "title font": px(q("h1"), "font"),
    "sub font": px(q(".by"), "font"),
    "blob box": box(q("canvas.blob")),
    "seek box": box(q(".seek canvas")),
    "times font": px(q(".times"), "font"),
    // The artifact's row is .ctl itself; ours nests the trio in .transport.
    "ctrls gap": px(q(".transport") || q(".ctl"), "gap"),
    "step box": box(q(".ctl .b")),
    "play box": box(q(".ctl .b.go")),
    "play fill": px(q(".ctl .b.go"), "background-image"),
    "vol box": box(q(".vol")),
    "range track": px(q('input[type="range"]'), "appearance"),
    // Dense views only (the list IS the view in RESULTS/QUEUE).
    "list box": box(q(".list")),
    "list head font": px(q(".lh"), "font"),
    "row box": box(q(".list .row")),
    "row thumb box": box(q(".list .row .t")),
    "row thumb radius": px(q(".list .row .t"), "border-radius"),
    "row thumb bg": px(q(".list .row .t"), "background-color"),
    "row title font": px(q(".list .row .n"), "font"),
    "row meta font": px(q(".list .row .m"), "font"),
    "row action box": box(q(".list .row .ic")),
    "tabs box": box(q(".tabs")),
    "tab font": px(q(".tabs button"), "font"),
    "status bar box": box(q(".sb")),
    "status bar font": px(q(".sb"), "font"),
    "status bar bg": px(q(".sb"), "background-color"),
    "unbounded loaded": !!(
      document.fonts && document.fonts.check("700 17px Unbounded")
    ),
  };
}

/** Geometry the browser can verify and the eye keeps missing. */
function geometry() {
  const box = (sel) => {
    const el = document.querySelector(sel);
    if (!el) return null;
    const r = el.getBoundingClientRect();
    if (!r.width && !r.height) return null;
    return { l: r.left, t: r.top, r: r.right, b: r.bottom, w: r.width, h: r.height };
  };
  const win = document.querySelector(".win");
  const list = document.querySelector(".list");
  return {
    view: (() => {
      const on = document.querySelector('.tabs button[aria-pressed="true"]');
      return on ? on.textContent.trim().split(/\s+/)[0] : null;
    })(),
    win: box(".win"),
    winScroll: win ? { h: win.clientHeight, sh: win.scrollHeight } : null,
    vol: box(".vol"),
    ctl: box(".ctl"),
    play: box(".ctl .b.go"),
    list: box(".list"),
    listScroll: list
      ? { sh: list.scrollHeight, ch: list.clientHeight, st: list.scrollTop }
      : null,
    // Whatever follows the body: the list in dense views, the tab bar in NOW.
    // Nothing in the body may cross its top edge.
    boundary: box(".list") || box(".tabs") || box(".sb"),
  };
}

/** Centring, overlap, fit and list scrolling. Returns the failure count.
 * opts.centre — only the NOW view centres its transport; the design's dense
 * header left-aligns it, so asserting a centre line there would be fiction. */
async function layout(page, label, opts = {}) {
  const { expectFits = true, centre = true } = opts;
  const g = await page.evaluate(geometry);
  const out = [];
  const mid = g.win.l + g.win.w / 2;
  if (centre && g.play && Math.abs(g.play.l + g.play.w / 2 - mid) > 1)
    out.push(`play pill off-centre by ${(g.play.l + g.play.w / 2 - mid).toFixed(1)}px`);
  if (g.vol && Math.abs(g.vol.l + g.vol.w / 2 - mid) > 1)
    out.push(`volume row off-centre by ${(g.vol.l + g.vol.w / 2 - mid).toFixed(1)}px`);
  for (const [name, b] of [
    ["volume row", g.vol],
    ["transport", g.ctl],
  ])
    if (b && g.boundary && b.b > g.boundary.t + 0.5)
      out.push(`${name} overlaps what follows it by ${(b.b - g.boundary.t).toFixed(1)}px`);
  // The list is its own scroll container: content taller than the window must
  // move on wheel input rather than being clipped.
  if (g.listScroll && g.listScroll.sh > g.listScroll.ch + 1) {
    await page.hover(".list .row");
    await page.mouse.wheel(0, 200);
    await page.waitForTimeout(150);
    const after = await page.evaluate(
      () => document.querySelector(".list").scrollTop,
    );
    if (after <= 0) out.push("results list will not scroll on wheel");
  }
  if (expectFits && g.winScroll.sh > g.winScroll.h + 1)
    out.push(`card overflows by ${g.winScroll.sh - g.winScroll.h}px at the design size`);

  // The command pill IS the search box, so it owns the focus ring: focusing it
  // lights the pill, and the input inside must not draw a square outline.
  // Blur first: an earlier check may have left the field focused, and the
  // "before" reading has to be the resting state for this to mean anything.
  await page.evaluate(() => {
    if (document.activeElement instanceof HTMLElement) document.activeElement.blur();
  });
  await page.waitForTimeout(300);
  const ringBefore = await page.evaluate(() =>
    getComputedStyle(document.querySelector(".cmd")).borderTopColor,
  );
  await page.focus("#q");
  await page.waitForTimeout(350); // .cmd transitions its border over .2s
  const ring = await page.evaluate(() => {
    const input = getComputedStyle(document.querySelector("#q"));
    const pill = document.querySelector(".cmd");
    return {
      pill: getComputedStyle(pill).borderTopColor,
      style: input.outlineStyle,
      width: input.outlineWidth,
      matches: pill.matches(":focus-within"),
    };
  });
  if (!ring.matches) out.push("command pill never matches :focus-within");
  if (ring.pill === ringBefore) out.push("command pill does not light up on focus");
  if (ring.style !== "none" && parseFloat(ring.width) > 0)
    out.push(`search input draws its own ${ring.width} ${ring.style} outline`);
  await page.evaluate(() => {
    if (document.activeElement instanceof HTMLElement) document.activeElement.blur();
  });

  // Always print what was measured, not just what failed: the numbers are the
  // evidence for the centring/overlap/scroll fixes.
  const off = (b) =>
    `${b.l + b.w / 2 - mid >= 0 ? "+" : ""}${(b.l + b.w / 2 - mid).toFixed(1)}`;
  const facts = [
    g.view && `view ${g.view}`,
    centre && g.play && `play ${off(g.play)}px`,
    g.vol && `vol ${off(g.vol)}px`,
    g.winScroll && `card ${g.winScroll.sh}/${g.winScroll.h}px`,
    g.listScroll && `list ${g.listScroll.sh}/${g.listScroll.ch}px`,
    `focus pill ${ringBefore === ring.pill ? "unchanged" : "lit"}, input outline ${ring.style}`,
  ].filter(Boolean);
  console.log(
    `\nlayout ${label}: ${out.length ? out.join("; ") : "clean"}  (${facts.join(", ")})`,
  );
  return out.length;
}

const ALLOWED = new Set([
  // The window is transparent so the card's rounded corners show the desktop;
  // the artifact paints its own opaque backdrop.
  "body bg",
]);

function report(label, art, app, extraAllowed) {
  const keys = Object.keys(app);
  let fails = 0;
  console.log(`\n== ${label} ==`);
  console.log(
    `${"field".padEnd(20)}${"artifact".padEnd(34)}${"app".padEnd(34)}`,
  );
  for (const k of keys) {
    const a = art ? art[k] : undefined;
    const allowed = ALLOWED.has(k) || (extraAllowed && extraAllowed.has(k));
    const same = a === undefined ? true : String(a) === String(app[k]);
    if (!same && !allowed) fails++;
    const mark = a === undefined ? "?" : same ? "ok" : allowed ? "…" : "DIFF";
    console.log(
      `${k.padEnd(20)}${String(a ?? "-").padEnd(34)}${String(app[k]).padEnd(34)}${mark}`,
    );
  }
  console.log(fails ? `${fails} DIFF${fails > 1 ? "S" : ""}` : "all match");
  return fails;
}

async function main() {
  const { chromium } = require(path.join(PLAYWRIGHT_HOME, "node_modules", "playwright"));
  fs.mkdirSync(SHOTS, { recursive: true });
  const browser = await chromium.launch();

  // -- baseline: the design artifact, untouched -----------------------------
  let artNow = null;
  let art37 = null;
  let artDense = null;
  if (fs.existsSync(BASELINE)) {
    const ctx = await browser.newContext({
      viewport: ARTIFACT_VIEWPORT,
      deviceScaleFactor: 2,
    });
    const page = await ctx.newPage();
    await page.goto(pathToFileURL(BASELINE).href);
    await page.waitForSelector("canvas.blob");
    // The artifact opens on RESULTS with a seeded query; its NOW view is the
    // card, so the two probes below cover both halves of the design.
    await page.screenshot({ path: path.join(SHOTS, "baseline-artifact-results.png") });
    await page.waitForTimeout(1500); // webfonts + a few animation frames
    artDense = await page.evaluate(probe);
    await page.click(".tabs button:nth-of-type(1)"); // NOW
    await page.waitForTimeout(500);
    await page.screenshot({ path: path.join(SHOTS, "baseline-artifact-now.png") });
    artNow = await page.evaluate(probe);
    // Same view, retinted to a track's hue: proves every hue-derived value
    // (palette, gradients) matches the app's once the hue matches.
    await page.evaluate(() =>
      document.documentElement.style.setProperty("--h", "37"),
    );
    await page.waitForTimeout(300);
    await page.screenshot({ path: path.join(SHOTS, "baseline-artifact-h37.png") });
    art37 = await page.evaluate(probe);
    await ctx.close();
  } else {
    console.log(`no baseline at ${BASELINE} — comparing nothing`);
  }

  // -- the app, driven through its real wiring ------------------------------
  const ctx = await browser.newContext({
    viewport: APP_VIEWPORT,
    deviceScaleFactor: 2,
  });
  await ctx.addInitScript(stub);
  const page = await ctx.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(`pageerror: ${String(e)}`));
  page.on("console", (m) => {
    if (m.type() === "error") errors.push(`console: ${m.text()}`);
  });
  page.on("response", (r) => {
    if (r.status() >= 400) errors.push(`${r.status()} ${r.url()}`);
  });

  await page.goto(APP);
  await page.waitForSelector("canvas.blob");
  await page.waitForTimeout(1500);
  await page.screenshot({ path: path.join(SHOTS, "app-now.png") });

  let fails = report("NOW vs artifact NOW", artNow, await page.evaluate(probe));
  fails += await layout(page, `NOW at the design size ${APP_VIEWPORT.width}x${APP_VIEWPORT.height}`);

  // A window dragged shorter than the design: the card as a whole has to
  // scroll instead of spilling over its own status bar.
  await page.setViewportSize({ width: 312, height: 528 });
  await page.waitForTimeout(200);
  await page.screenshot({ path: path.join(SHOTS, "app-squeezed-312x528.png") });
  fails += await layout(page, "squeezed to 312x528", { expectFits: false });
  await page.setViewportSize(APP_VIEWPORT);
  await page.waitForTimeout(200);

  // mini mode. A browser cannot shrink the OS window the way setSize does, so
  // the viewport stands in for it: 390x206 is the window MINI asks for.
  await page.click('[aria-label="Mini mode"]');
  await page.setViewportSize({ width: MINI_SIZE.w, height: MINI_SIZE.h });
  await page.waitForTimeout(400);
  await page.screenshot({ path: path.join(SHOTS, "app-mini.png") });
  const mini = await page.evaluate(probe);
  const hid = (v) => v || "hidden";
  console.log(
    `\nmini: blob ${mini["blob box"]} (design says 64x64), cmd ${hid(
      mini["cmd box"],
    )}, list ${hid(mini["list box"])}, vol ${hid(
      mini["vol box"],
    )}, tabs ${hid(mini["tabs box"])}`,
  );
  await page.click('[aria-label="Full player"]');
  await page.setViewportSize(APP_VIEWPORT);
  await page.waitForTimeout(300);

  // search -> the RESULTS view: the list replaces the card
  await page.fill("#q", "lofi");
  await page.waitForTimeout(700); // the app's own search debounce
  await page.click(".tabs button:nth-of-type(2)");
  await page.waitForSelector(".list .row");
  await page.waitForTimeout(300);
  await page.screenshot({ path: path.join(SHOTS, "app-results.png") });
  fails += report(
    "RESULTS vs artifact RESULTS",
    artDense,
    await page.evaluate(probe),
  );
  fails += await layout(page, "RESULTS at the design size", { centre: false });

  // the live row: hue-shifted art, "Channel · ● LIVE", and the accent colour
  // the design reserves for live (ours is the only side with real live data)
  const live = await page.evaluate(() => {
    const el = document.querySelector(".list .lv");
    const row = el && el.closest(".row");
    return el
      ? {
          text: el.textContent,
          meta: row.querySelector(".m").textContent,
          color: getComputedStyle(el).color,
        }
      : null;
  });
  console.log(`\nlive row: "${live?.meta}" ${live?.color}`);
  if (!live || live.meta !== "Lofi Girl · ● LIVE" || live.color !== "rgb(255, 107, 125)") {
    console.log("live row is not rendered as the design describes it");
    fails++;
  }

  // the searching state: five shimmer rows and a pulsing badge mid-flight
  await page.fill("#q", "midnight");
  await page.keyboard.press("Enter"); // Enter jumps to RESULTS
  await page.waitForTimeout(200);
  const mid = await page.evaluate(() => ({
    sr: document.querySelectorAll(".sr").length,
    head: document.querySelector(".lh-right")?.textContent ?? "",
    badge: document.querySelector(".tabs button:nth-of-type(2) b")?.className ?? "",
  }));
  console.log(
    `searching: ${mid.sr} shimmer rows, header "${mid.head}", badge class "${mid.badge}"`,
  );
  if (mid.sr !== 5 || !/searching/.test(mid.head) || !mid.badge.includes("dim")) {
    console.log("the searching state does not match the design");
    fails++;
  }
  await page.waitForSelector(".list .row");
  await page.waitForTimeout(400);

  // the queue view, with two rows added from the results (the list head is a
  // div too, so nth-of-type would land on the same row twice)
  await page.locator(".list .row .ic").nth(0).click();
  await page.locator(".list .row .ic").nth(1).click();
  await page.click(".tabs button:nth-of-type(3)");
  await page.waitForTimeout(300);
  await page.screenshot({ path: path.join(SHOTS, "app-queue.png") });
  const queued = await page.evaluate(() => ({
    view: document.querySelector('.tabs button[aria-pressed="true"]').textContent,
    rows: document.querySelectorAll(".list .row").length,
    heads: document.querySelector(".list .lh").textContent,
  }));
  console.log(
    `\nqueue: ${queued.rows} row(s), view "${queued.view.trim()}", header "${queued.heads.trim()}"`,
  );
  if (queued.rows !== 2) {
    console.log(`queue view shows ${queued.rows} rows, expected 2`);
    fails++;
  }

  // play the first hit -> per-track re-tint, then a real state push, then Esc
  await page.click(".tabs button:nth-of-type(2)");
  await page.waitForTimeout(200);
  await page.click(".list .row");
  await page.evaluate(() =>
    window.__emit("player://state", {
      playing: true,
      paused: false,
      loading: false,
      position: 42,
      duration: 213,
      seekable: true,
      volume: 80,
      title: null,
      ended: false,
      error: null,
    }),
  );
  await page.waitForTimeout(800);
  await page.screenshot({ path: path.join(SHOTS, "app-playing.png") });
  await page.keyboard.press("Escape"); // Esc returns to NOW from anywhere
  await page.waitForTimeout(600);
  await page.screenshot({ path: path.join(SHOTS, "app-playing-now.png") });
  const playing = await page.evaluate(probe);
  fails += report("NOW playing vs artifact at hue 37", art37, playing);
  console.log(`re-tint: --h is ${playing["hue var"]} (expected 37 for dQw4w9WgXcQ)`);
  fails += await layout(page, "NOW playing at the design size");

  await page.setViewportSize(APP_VIEWPORT);
  await page.waitForTimeout(300);

  await ctx.close();
  await browser.close();

  console.log(`\nscreenshots: ${SHOTS}`);
  if (errors.length) {
    console.log(`\n${errors.length} app console/page error(s):`);
    for (const e of errors.slice(0, 12)) console.log(`  ${e}`);
  } else {
    console.log("\napp console: clean");
  }
  process.exitCode = fails ? 1 : 0;
}

main().catch((e) => {
  console.error(e);
  process.exit(2);
});
