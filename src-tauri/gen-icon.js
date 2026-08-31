// Genera un icon.ico minimo y valido (32x32, BGRA 32-bit) sin dependencias.
// Fondo azul acero (#5980a6) con un marco blueprint claro. Placeholder.
const fs = require("fs");
const path = require("path");

const S = 32;
const accent = [0xa6, 0x80, 0x59]; // BGR de #5980a6
const light = [0xdf, 0xdf, 0xdf];

// Pixeles BGRA, fila inferior primero (formato DIB).
const px = Buffer.alloc(S * S * 4);
for (let y = 0; y < S; y++) {
  for (let x = 0; x < S; x++) {
    const i = (y * S + x) * 4;
    const border = x < 2 || y < 2 || x >= S - 2 || y >= S - 2;
    // marcas de esquina en +
    const near = (a, b) => Math.abs(a - b) <= 3;
    const corner =
      ((near(x, 0) || near(x, S - 1)) && (y < 6 || y >= S - 6)) ||
      ((near(y, 0) || near(y, S - 1)) && (x < 6 || x >= S - 6));
    const c = border || corner ? light : accent;
    px[i] = c[0];
    px[i + 1] = c[1];
    px[i + 2] = c[2];
    px[i + 3] = 0xff;
  }
}
// DIB espera filas de abajo hacia arriba: invertir verticalmente.
const dib = Buffer.alloc(px.length);
for (let y = 0; y < S; y++) {
  px.copy(dib, (S - 1 - y) * S * 4, y * S * 4, (y + 1) * S * 4);
}

// BITMAPINFOHEADER (40 bytes). Altura = 2*S (imagen + mascara AND).
const bih = Buffer.alloc(40);
bih.writeUInt32LE(40, 0);
bih.writeInt32LE(S, 4);
bih.writeInt32LE(S * 2, 8);
bih.writeUInt16LE(1, 12);
bih.writeUInt16LE(32, 14);
// Mascara AND (1bpp), todo 0 = opaco. Fila alineada a 4 bytes: 4 bytes * S.
const andMask = Buffer.alloc(4 * S, 0);
const image = Buffer.concat([bih, dib, andMask]);

// ICONDIR (6) + ICONDIRENTRY (16)
const dir = Buffer.alloc(6);
dir.writeUInt16LE(0, 0);
dir.writeUInt16LE(1, 2);
dir.writeUInt16LE(1, 4);
const entry = Buffer.alloc(16);
entry.writeUInt8(S, 0);
entry.writeUInt8(S, 1);
entry.writeUInt8(0, 2);
entry.writeUInt8(0, 3);
entry.writeUInt16LE(1, 4);
entry.writeUInt16LE(32, 6);
entry.writeUInt32LE(image.length, 8);
entry.writeUInt32LE(6 + 16, 12);

const ico = Buffer.concat([dir, entry, image]);
const out = path.join(__dirname, "icons", "icon.ico");
fs.mkdirSync(path.dirname(out), { recursive: true });
fs.writeFileSync(out, ico);
console.log("icon.ico escrito:", out, ico.length, "bytes");
