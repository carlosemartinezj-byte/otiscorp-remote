// Genera las imagenes de marca del instalador NSIS (header + sidebar), con el
// mismo azul acero y anillo "O" del icono de la app, para que el asistente de
// instalacion se vea como OtisCorp en vez del wizard gris generico.
const fs = require("fs");
const path = require("path");

const bg = [0x59, 0x80, 0xa6]; // --color-accent #5980a6 (steel blue)
const fg = [0xf2, 0xf2, 0xf3]; // --color-bg (paper)

// BMP de 24bpp, bottom-up, sin compresion.
function writeBmp(outPath, w, h, pixelAt) {
  const rowSize = Math.ceil((w * 3) / 4) * 4;
  const pixelArraySize = rowSize * h;
  const fileSize = 54 + pixelArraySize;

  const buf = Buffer.alloc(fileSize);
  buf.write("BM", 0, "ascii");
  buf.writeUInt32LE(fileSize, 2);
  buf.writeUInt32LE(54, 10); // offset a datos de pixel
  buf.writeUInt32LE(40, 14); // tamano DIB header
  buf.writeInt32LE(w, 18);
  buf.writeInt32LE(h, 22);
  buf.writeUInt16LE(1, 26); // planos
  buf.writeUInt16LE(24, 28); // bits por pixel
  buf.writeUInt32LE(0, 30); // sin compresion
  buf.writeUInt32LE(pixelArraySize, 34);

  let o = 54;
  for (let y = h - 1; y >= 0; y--) {
    for (let x = 0; x < w; x++) {
      const [r, g, b] = pixelAt(x, y);
      buf[o++] = b; buf[o++] = g; buf[o++] = r;
    }
    const pad = rowSize - w * 3;
    for (let p = 0; p < pad; p++) buf[o++] = 0;
  }
  fs.writeFileSync(outPath, buf);
  console.log("bmp generado:", outPath, fileSize, "bytes");
}

function ring(x, y, cx, cy, rOuter, rInner) {
  const d = Math.hypot(x - cx, y - cy);
  return d <= rOuter && d >= rInner;
}

const outDir = path.join(__dirname, "icons");

// Sidebar (bienvenida/fin): 164x314, logo grande centrado en la mitad superior.
writeBmp(path.join(outDir, "nsis-sidebar.bmp"), 164, 314, (x, y) => {
  const cx = 82, cy = 110, rOuter = 46, rInner = 32;
  return ring(x, y, cx, cy, rOuter, rInner) ? fg : bg;
});

// Header (paginas intermedias): 150x57, logo pequeno a la izquierda.
writeBmp(path.join(outDir, "nsis-header.bmp"), 150, 57, (x, y) => {
  const cx = 28, cy = 28, rOuter = 18, rInner = 12;
  return ring(x, y, cx, cy, rOuter, rInner) ? fg : [0xf2, 0xf2, 0xf3];
});
