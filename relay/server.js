'use strict';
// OtisCorp Remote — servidor relay / rendezvous.
//
// Empareja dos clientes por su ID de 9 digitos y hace de puente transparente
// entre ellos. No entiende el protocolo de sesion (frames MJPEG, eventos de
// entrada): una vez emparejados, copia bytes en crudo en ambos sentidos.
//
// Handshake (una linea de texto terminada en '\n'):
//   HOST <id>   -> el equipo controlado se registra y espera. Cuando llega un
//                  viewer, el relay le envia "PEER\n" y a partir de ahi la misma
//                  conexion es el tunel de sesion.
//   JOIN <id>   -> el visor pide conectar con ese id. Si hay host, recibe "OK\n"
//                  y queda entubado con el host; si no, recibe "NOHOST\n".
//
// Fly.io termina TLS en el borde (puerto 443), asi que el tramo por internet va
// cifrado aunque aqui escuchemos en claro dentro de la red interna de Fly.

const net = require('net');

const PORT = parseInt(process.env.PORT || '8080', 10);
const MAX_LINE = 64; // el handshake es corto; corta a clientes basura
const HANDSHAKE_TIMEOUT_MS = 20000; // sin "HOST/JOIN\n" en 20s -> fuera
const KEEPALIVE_MS = 25000; // sonda TCP: detecta peers muertos y mantiene vivo el NAT

// id -> socket del host en espera
const hosts = new Map();

function log(...a) {
  console.log(new Date().toISOString(), ...a);
}

function validId(id) {
  return /^[0-9]{6,12}$/.test(id);
}

const server = net.createServer((sock) => {
  sock.setNoDelay(true);
  // Keepalive: si el host se queda esperando "PEER" sin trafico, esto mantiene
  // viva la asignacion NAT del cliente y detecta si de verdad se cayo (en vez de
  // dejar un socket muerto ocupando sitio en el Map de hosts).
  sock.setKeepAlive(true, KEEPALIVE_MS);

  let buf = Buffer.alloc(0);
  let handshaked = false;

  // Medio-conexiones (TLS a medias, escaneos, health checks colgados) no pueden
  // acumularse: si no completan el handshake pronto, se cierran.
  const hsTimer = setTimeout(() => {
    if (!handshaked) {
      log('handshake no completado, cerrando');
      sock.destroy();
    }
  }, HANDSHAKE_TIMEOUT_MS);

  const onData = (chunk) => {
    if (handshaked) return; // ya entubado; el pipe se encarga
    buf = Buffer.concat([buf, chunk]);
    const nl = buf.indexOf(0x0a);
    if (nl === -1) {
      if (buf.length > MAX_LINE) { sock.destroy(); }
      return;
    }
    const line = buf.slice(0, nl).toString('utf8').trim();
    const rest = buf.slice(nl + 1); // posibles bytes de sesion ya recibidos
    handshaked = true;
    clearTimeout(hsTimer);
    sock.removeListener('data', onData);
    handleCommand(sock, line, rest);
  };
  sock.on('data', onData);
  sock.on('close', () => clearTimeout(hsTimer));

  sock.on('error', () => { /* se limpia en 'close' */ });
});

function handleCommand(sock, line, rest) {
  const sp = line.indexOf(' ');
  const cmd = (sp === -1 ? line : line.slice(0, sp)).toUpperCase();
  const id = sp === -1 ? '' : line.slice(sp + 1).trim();

  if (!validId(id)) {
    sock.end('ERR bad-id\n');
    return;
  }

  if (cmd === 'HOST') {
    const prev = hosts.get(id);
    if (prev && prev !== sock && !prev.destroyed) prev.destroy();
    hosts.set(id, sock);
    log('HOST registrado', id, '(hosts activos:', hosts.size + ')');
    sock.on('close', () => {
      if (hosts.get(id) === sock) hosts.delete(id);
      log('HOST desconectado', id, '(hosts activos:', hosts.size + ')');
    });
    // El host espera; no reenviamos "rest" (no deberia haber datos aun).
    return;
  }

  if (cmd === 'JOIN') {
    const host = hosts.get(id);
    if (!host || host.destroyed) {
      sock.end('NOHOST\n');
      log('JOIN sin host', id);
      return;
    }
    hosts.delete(id); // sesion 1:1; el host se re-registrara al terminar
    log('JOIN emparejado', id);
    host.write('PEER\n');
    sock.write('OK\n');
    pipePair(host, sock, id, rest);
    return;
  }

  sock.end('ERR bad-cmd\n');
}

// Puente transparente bidireccional entre host y viewer.
function pipePair(host, viewer, id, viewerRest) {
  let closed = false;
  const close = () => {
    if (closed) return;
    closed = true;
    if (!host.destroyed) host.destroy();
    if (!viewer.destroyed) viewer.destroy();
    log('sesion cerrada', id);
  };
  host.on('data', (d) => { if (!viewer.write(d)) host.pause(); });
  viewer.on('drain', () => host.resume());
  viewer.on('data', (d) => { if (!host.write(d)) viewer.pause(); });
  host.on('drain', () => viewer.resume());
  host.on('close', close);
  viewer.on('close', close);
  host.on('error', close);
  viewer.on('error', close);
  // Reenvia lo que el viewer hubiera mandado pegado al JOIN (normalmente nada).
  if (viewerRest && viewerRest.length) host.write(viewerRest);
}

server.on('error', (e) => {
  log('error del servidor', e.message);
  process.exit(1);
});

// Un error suelto (socket que revienta en un momento raro) NO debe tumbar el
// relay entero y cortarle la sesion a todo el mundo.
process.on('uncaughtException', (e) => log('uncaughtException:', (e && e.message) || e));
process.on('unhandledRejection', (e) => log('unhandledRejection:', (e && e.message) || e));

server.listen(PORT, '0.0.0.0', () => {
  log('OtisCorp relay escuchando en', PORT);
});
