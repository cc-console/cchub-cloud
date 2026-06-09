// Generates a 1024x1024 RGBA source icon (app/icon-src.png) with no deps —
// a dark rounded square with a blue ">_" console prompt. Run once; `tauri icon`
// derives every platform size from the output.
const zlib = require("zlib");
const fs = require("fs");

const S = 1024;
const buf = Buffer.alloc(S * S * 4); // RGBA, transparent

const hex = (h) => [parseInt(h.slice(0,2),16), parseInt(h.slice(2,4),16), parseInt(h.slice(4,6),16)];
const BG = hex("0c0f14");
const FG = hex("4f8cff");

function px(x, y, [r, g, b], a = 255) {
  if (x < 0 || y < 0 || x >= S || y >= S) return;
  const i = (y * S + x) * 4;
  buf[i] = r; buf[i+1] = g; buf[i+2] = b; buf[i+3] = a;
}
function fillRect(x0, y0, w, h, c) {
  for (let y = y0; y < y0 + h; y++) for (let x = x0; x < x0 + w; x++) px(x, y, c);
}

// Rounded background square.
const R = 180, M = 40, side = S - 2 * M;
for (let y = 0; y < S; y++) for (let x = 0; x < S; x++) {
  const ix = x - M, iy = y - M;
  if (ix < 0 || iy < 0 || ix >= side || iy >= side) continue;
  const cx = Math.min(Math.max(ix, R), side - R);
  const cy = Math.min(Math.max(iy, R), side - R);
  const d = Math.hypot(ix - cx, iy - cy);
  if (d <= R) px(x, y, BG);
}

// ">" chevron (two thick strokes) centred-left.
const t = 56;                 // stroke thickness
const cxp = 360, cyp = 512;   // chevron tip x, vertical centre
const len = 200;
for (let k = 0; k < len; k++) {
  fillRect(cxp - 220 + k, cyp - 200 + k, t, t, FG); // upper arm ↘
  fillRect(cxp - 220 + k, cyp + 200 - k - t, t, t, FG); // lower arm ↗
}
// "_" underscore to the right.
fillRect(470, 660, 230, t, FG);

// --- PNG encode (RGBA, no filtering) ---
const raw = Buffer.alloc(S * (S * 4 + 1));
for (let y = 0; y < S; y++) {
  raw[y * (S * 4 + 1)] = 0; // filter type 0
  buf.copy(raw, y * (S * 4 + 1) + 1, y * S * 4, (y + 1) * S * 4);
}
function chunk(type, data) {
  const len = Buffer.alloc(4); len.writeUInt32BE(data.length);
  const td = Buffer.concat([Buffer.from(type), data]);
  const crc = Buffer.alloc(4); crc.writeUInt32BE(crc32(td) >>> 0);
  return Buffer.concat([len, td, crc]);
}
function crc32(b) {
  let c = ~0;
  for (let i = 0; i < b.length; i++) {
    c ^= b[i];
    for (let j = 0; j < 8; j++) c = (c >>> 1) ^ (0xEDB88320 & -(c & 1));
  }
  return ~c;
}
const ihdr = Buffer.alloc(13);
ihdr.writeUInt32BE(S, 0); ihdr.writeUInt32BE(S, 4);
ihdr[8] = 8; ihdr[9] = 6; // 8-bit, RGBA
const png = Buffer.concat([
  Buffer.from([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]),
  chunk("IHDR", ihdr),
  chunk("IDAT", zlib.deflateSync(raw)),
  chunk("IEND", Buffer.alloc(0)),
]);
fs.writeFileSync(__dirname + "/../icon-src.png", png);
console.log("wrote app/icon-src.png (" + png.length + " bytes)");
