// Genera icons/icon.png (1024x1024) — icono fuente para todas las plataformas.
// Azul acero del sistema Industry con un anillo blanco (marca "O" de OtisCorp).
// Sin dependencias: construye el PNG a mano con zlib.
const fs = require("fs");
const zlib = require("zlib");
const path = require("path");

const S = 1024;
const bg = [0x59, 0x80, 0xa6]; // --color-accent #5980a6
const fg = [0xf2, 0xf2, 0xf3]; // --color-bg (paper)

const cx = S / 2, cy = S / 2;
const rOuter = 360, rInner = 250;

const raw = Buffer.alloc(S * (S * 4 + 1));
let o = 0;
for (let y = 0; y < S; y++) {
  raw[o++] = 0; // filtro de fila = None
  for (let x = 0; x < S; x++) {
    const d = Math.hypot(x - cx, y - cy);
    const ring = d <= rOuter && d >= rInner;
    const c = ring ? fg : bg;
    raw[o++] = c[0];
    raw[o++] = c[1];
    raw[o++] = c[2];
    raw[o++] = 0xff;
  }
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length, 0);
  const t = Buffer.from(type, "ascii");
  const body = Buffer.concat([t, data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body) >>> 0, 0);
  return Buffer.concat([len, body, crc]);
}

function crc32(buf) {
  let c = ~0;
  for (let i = 0; i < buf.length; i++) {
    c ^= buf[i];
    for (let k = 0; k < 8; k++) c = (c >>> 1) ^ (0xedb88320 & -(c & 1));
  }
  return ~c;
}

const sig = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]);
const ihdr = Buffer.alloc(13);
ihdr.writeUInt32BE(S, 0);
ihdr.writeUInt32BE(S, 4);
ihdr[8] = 8; // bit depth
ihdr[9] = 6; // color type RGBA
const idat = zlib.deflateSync(raw);
const png = Buffer.concat([
  sig,
  chunk("IHDR", ihdr),
  chunk("IDAT", idat),
  chunk("IEND", Buffer.alloc(0)),
]);

const out = path.join(__dirname, "icons", "icon.png");
fs.writeFileSync(out, png);
console.log("icono generado:", out, png.length, "bytes");
