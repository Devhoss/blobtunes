/* Recover the tray artwork from a previously built exe.
 *
 * tauri::include_image! decodes the PNG at compile time and embeds raw RGBA as
 * a byte array in .rdata. The tray images are one-colour, so every pixel has
 * R === G === B, which is a strong enough signature to find them again after
 * the source PNGs are lost. Re-encoded here as PNGs.
 *
 *   node tools/recover-tray.cjs [exe ...]     # writes src-tauri/icons/tray-*.png
 */
const fs = require("node:fs");
const path = require("node:path");
const zlib = require("node:zlib");

const ICONS = path.join(__dirname, "..", "src-tauri", "icons");
const exes = process.argv.slice(2).length
  ? process.argv.slice(2)
  : [
      path.join(__dirname, "..", "portable", "blobtunes.exe"),
      path.join(__dirname, "..", "src-tauri", "target", "release", "blobtunes.exe"),
    ];

/** A run where every pixel's R=G=B and the alpha channel varies: a one-colour
 * icon. Returns { off, len, w, h } candidates at plausible icon sizes. */
function scan(buf) {
  const hits = [];
  const sizes = [22 * 22 * 4, 32 * 32 * 4, 44 * 44 * 4];
  for (let off = 0; off + 44 * 44 * 4 < buf.length; off += 4) {
    let n = 0;
    let max = 0;
    while (off + n + 4 <= buf.length && n < 44 * 44 * 4) {
      const a = buf[off + n];
      const b = buf[off + n + 1];
      const c = buf[off + n + 2];
      if (a !== b || b !== c) break;
      max = Math.max(max, a);
      n += 4;
    }
    for (const len of sizes) {
      if (n >= len && n < len + 4096) {
        // trim to the exact grid: the run may start mid-literal
        for (let s = 0; s <= 4; s += 4) {
          const start = off + s;
          if (start + len > buf.length) continue;
          const px = buf.subarray(start, start + len);
          const side = Math.sqrt(len / 4);
          const alphaAt = (x, y) => px[(y * side + x) * 4 + 3];
          // a blob: transparent corners, something in the middle
          const corners =
            alphaAt(0, 0) + alphaAt(side - 1, 0) + alphaAt(0, side - 1) + alphaAt(side - 1, side - 1);
          const mid = alphaAt(side >> 1, side >> 1) + alphaAt(side >> 1, (side >> 1) + 4);
          if (corners < 64 && mid > 200 && max > 200) {
            hits.push({ off: start, len, side, px });
          }
          break;
        }
      }
    }
  }
  return hits;
}

function png(rgba, side) {
  const raw = Buffer.alloc((side * 4 + 1) * side);
  for (let y = 0; y < side; y++) {
    raw[y * (side * 4 + 1)] = 0; // filter: none
    rgba.copy(raw, y * (side * 4 + 1) + 1, y * side * 4, (y + 1) * side * 4);
  }
  const chunk = (type, data) => {
    const head = Buffer.alloc(8);
    head.writeUInt32BE(data.length, 0);
    head.write(type, 4, "ascii");
    const crcTable = [];
    for (let n = 0; n < 256; n++) {
      let c = n;
      for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
      crcTable[n] = c >>> 0;
    }
    let crc = 0xffffffff;
    for (const byte of Buffer.concat([head.subarray(4), data]))
      crc = crcTable[(crc ^ byte) & 0xff] ^ (crc >>> 8);
    const tail = Buffer.alloc(4);
    tail.writeUInt32BE((crc ^ 0xffffffff) >>> 0, 0);
    return Buffer.concat([head, data, tail]);
  };
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(side, 0);
  ihdr.writeUInt32BE(side, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", zlib.deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

if (process.argv.includes("--dump")) {
  const buf = fs.readFileSync(exes[0]);
  for (let off = 0; off + 4096 < buf.length; off += 4) {
    let n = 0;
    while (off + n + 4 <= buf.length && n < 8192) {
      const a = buf[off + n],
        b = buf[off + n + 1],
        c = buf[off + n + 2];
      if (a !== b || b !== c) break;
      n += 4;
    }
    if (n >= 1500) {
      const side = Math.round(Math.sqrt(n / 4));
      const alpha = (x, y) => buf[off + (y * side + x) * 4 + 3];
      console.log(
        `off ${off} run ${n} bytes (~${side}x${side}) rgb0=${buf[off]} corners=${alpha(0, 0)},${alpha(side - 1, side - 1)} mid=${alpha(side >> 1, side >> 1)}`,
      );
      off += n - 4;
    }
  }
  process.exit(0);
}

for (const file of exes) {
  if (!fs.existsSync(file)) {
    console.log(`skip (absent): ${file}`);
    continue;
  }
  const buf = fs.readFileSync(file);
  const hits = scan(buf);
  const seen = new Map();
  for (const h of hits) if (!seen.has(h.side)) seen.set(h.side, h);
  console.log(`\n${path.basename(file)} (${buf.length} bytes): ${hits.length} candidate(s)`);
  for (const [side, h] of [...seen].sort((a, b) => a[0] - b[0])) {
    const out = path.join(ICONS, `recovered-${side}.png`);
    fs.writeFileSync(out, png(h.px, side));
    console.log(`  ${side}x${side} at offset ${h.off} -> ${path.relative(process.cwd(), out)}`);
  }
}
