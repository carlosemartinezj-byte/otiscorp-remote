// OtisCorp Remote — servidor rendezvous (señalización) para conexiones P2P
// fuera de la LAN. Es una "guía telefónica": mapea ID -> conexión y reenvía el
// saludo inicial entre dos equipos para que hagan hole punching. NO transporta
// vídeo ni control (eso va P2P directo), así que consume casi nada: cabe en un
// plan gratuito (Fly.io / Render / Oracle Always Free).
//
// Protocolo (JSON por WebSocket):
//   cliente -> servidor  { "type":"register", "id":"731204998" }
//   servidor -> cliente  { "type":"registered", "id":"..." }
//   cliente -> servidor  { "type":"signal", "to":"402118553", "data":{...} }
//   servidor -> destino  { "type":"signal", "from":"731204998", "data":{...} }
//   servidor -> cliente  { "type":"peer-offline", "to":"402118553" }
//   servidor -> cliente  { "type":"error", "message":"..." }
//
// `data` es opaco para el servidor: lo definen los clientes (su endpoint público
// STUN, un nonce de hole punching, la petición de conexión, etc.).

const http = require("http");
const { WebSocketServer } = require("ws");

const PORT = process.env.PORT || 8080;

// id (9 dígitos) -> ws
const peers = new Map();

const server = http.createServer((req, res) => {
  // Endpoint de salud (para que el tier gratis no marque el servicio como caído).
  if (req.url === "/health" || req.url === "/") {
    res.writeHead(200, { "content-type": "text/plain" });
    res.end("OtisCorp rendezvous OK · peers=" + peers.size);
    return;
  }
  res.writeHead(404);
  res.end();
});

const wss = new WebSocketServer({ server });

function send(ws, obj) {
  if (ws.readyState === ws.OPEN) {
    ws.send(JSON.stringify(obj));
  }
}

wss.on("connection", (ws) => {
  ws.id = null;
  ws.isAlive = true;
  ws.on("pong", () => (ws.isAlive = true));

  ws.on("message", (raw) => {
    let msg;
    try {
      msg = JSON.parse(raw.toString());
    } catch (_) {
      return; // ignora basura
    }

    switch (msg.type) {
      case "register": {
        const id = String(msg.id || "").replace(/\D/g, "");
        if (id.length < 6) {
          send(ws, { type: "error", message: "id inválido" });
          return;
        }
        // Si el id ya estaba registrado por otra conexión vieja, la reemplazamos.
        const prev = peers.get(id);
        if (prev && prev !== ws) {
          try { prev.close(); } catch (_) {}
        }
        ws.id = id;
        peers.set(id, ws);
        send(ws, { type: "registered", id });
        break;
      }

      case "signal": {
        const to = String(msg.to || "").replace(/\D/g, "");
        const target = peers.get(to);
        if (!target) {
          send(ws, { type: "peer-offline", to });
          return;
        }
        // Reenvía el saludo al destino, etiquetado con quién lo manda.
        send(target, { type: "signal", from: ws.id || null, data: msg.data });
        break;
      }

      default:
        // tipos desconocidos: ignorar
        break;
    }
  });

  ws.on("close", () => {
    if (ws.id && peers.get(ws.id) === ws) {
      peers.delete(ws.id);
    }
  });

  ws.on("error", () => {
    try { ws.close(); } catch (_) {}
  });
});

// Ping periódico: mantiene vivas las conexiones y limpia las muertas.
const interval = setInterval(() => {
  wss.clients.forEach((ws) => {
    if (ws.isAlive === false) {
      return ws.terminate();
    }
    ws.isAlive = false;
    try { ws.ping(); } catch (_) {}
  });
}, 30000);

wss.on("close", () => clearInterval(interval));

server.listen(PORT, () => {
  console.log(`OtisCorp rendezvous escuchando en :${PORT}`);
});
