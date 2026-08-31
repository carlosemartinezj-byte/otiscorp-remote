// OtisCorp Remote — capa P2P por internet usando WebRTC (nativo del WebView).
//
// El WebView (Chromium/WebView2) trae un stack WebRTC maduro: hace STUN + hole
// punching + cifrado DTLS por su cuenta. Aqui solo:
//   - hablamos con el servidor rendezvous (señalizacion) por WebSocket,
//   - creamos la RTCPeerConnection y dos data channels:
//       "frames" (fiable, ordenado) para el video MJPEG troceado,
//       "input"  (fiable, ordenado) para raton/teclado,
//   - el host reenvia por "frames" los JPEG que emite el backend (evento
//     `local-frame`) TROCEADOS en paquetes pequeños, y aplica el control
//     recibido llamando a los comandos input_*.
//
// El video/control van DIRECTOS entre las dos PCs (o por el TURN si el NAT no
// deja hole punching); el rendezvous solo intercambia el saludo inicial.
//
// Por que troceado: un data channel SCTP tiene un limite de tamano por mensaje
// (~256 KB en Chromium, 64 KB si un extremo es viejo). Un JPEG de pantalla
// completa a menudo lo supera -> `send()` lanza y, si el error se traga, la
// pantalla del visor se queda NEGRA sin ninguna pista. Aqui cada frame se parte
// en trozos de 16 KB con una cabecera [magic|seq|idx|total] y el visor lo
// reensambla; los trozos van por un canal FIABLE y ORDENADO, asi que llegan
// todos y en orden.

window.OtisRTC = (function () {
  "use strict";
  const tauri = window.__TAURI__;
  const invoke = tauri ? tauri.core.invoke : async () => null;
  const listen = tauri ? tauri.event.listen : async () => () => {};

  // STUN (descubrir la IP publica) + TURN de reserva (relay cuando el NAT no
  // deja hole punching: CGNAT, NAT simetrico, wifi corporativo). El TURN publico
  // de OpenRelay es best-effort; para algo serio, pon uno propio desde Ajustes
  // (se guarda en localStorage "otis_ice" como el array `iceServers` en JSON).
  const DEFAULT_ICE = [
    { urls: "stun:stun.l.google.com:19302" },
    { urls: "stun:stun1.l.google.com:19302" },
    { urls: "stun:global.stun.twilio.com:3478" },
    {
      urls: [
        "turn:openrelay.metered.ca:80",
        "turn:openrelay.metered.ca:443",
        "turn:openrelay.metered.ca:443?transport=tcp",
      ],
      username: "openrelayproject",
      credential: "openrelayproject",
    },
  ];
  function iceConfig() {
    let servers = DEFAULT_ICE;
    try {
      const raw = localStorage.getItem("otis_ice");
      if (raw) {
        const parsed = JSON.parse(raw);
        if (Array.isArray(parsed) && parsed.length) {
          // Permite tanto reemplazar como AÑADIR a los de fabrica.
          servers = DEFAULT_ICE.concat(parsed);
        }
      }
    } catch (e) {
      console.warn("[rtc] otis_ice inválido, usando ICE por defecto", e);
    }
    return { iceServers: servers, iceCandidatePoolSize: 2 };
  }

  const LS_KEY = "otis_rendezvous";
  // Servidor rendezvous propio (Fly.io), preconfigurado de fabrica: el modo
  // internet queda activo desde la primera vez, sin que el usuario tenga que
  // pegar nada en Ajustes.
  const DEFAULT_RENDEZVOUS = "wss://otiscorp-rv-8842.fly.dev";

  const FRAME_BUFFER_LIMIT = 262_144; // bytes en cola: si se pasa, descarta el frame nuevo (evita acumular retraso)
  const FRAME_CHUNK = 16_000; // trozo de envio: por debajo del limite SCTP de cualquier WebView
  const FRAME_MAGIC = 0x46; // 'F' — primer byte de cada paquete de video

  let ws = null;
  let myId = null;
  let wsReady = false;
  let reconnectTimer = null;

  // Rol host (este equipo compartido)
  let hostPc = null;
  let hostFrames = null;
  let hostInput = null;
  let localFrameUnlisten = null;
  let sharingProfile = "ultralight";

  // Rol visor (controlando a otro)
  let viewPc = null;
  let viewFrames = null;
  let viewInput = null;
  let viewCbs = null;

  // Handler de aprobacion (lo fija app.js): recibe (peer, profile) y devuelve
  // una Promise<boolean> — true si el usuario de ESTE equipo autoriza que lo
  // controlen. Sin handler, se rechaza por seguridad (nunca auto-aceptar).
  let approvalHandler = null;
  function setApprovalHandler(fn) { approvalHandler = fn; }
  // Notifica a app.js cuando ESTE equipo empieza/termina de ser controlado,
  // para mostrar/ocultar la barra "te estan controlando · Terminar".
  let hostSessionCb = null;
  function setHostSessionHandler(fn) { hostSessionCb = fn; }
  function endHostSession() {
    if (hostPc) { try { hostPc.close(); } catch (_) {} }
    teardownHost();
  }

  function rendezvousUrl() {
    const saved = (localStorage.getItem(LS_KEY) || "").trim();
    return saved || DEFAULT_RENDEZVOUS;
  }
  function setRendezvous(url) {
    localStorage.setItem(LS_KEY, (url || "").trim());
  }
  function isConfigured() {
    return rendezvousUrl().length > 0;
  }

  // Consulta si un ID esta conectado al rendezvous ahora mismo (para el
  // punto de estado en la libreta de dispositivos). No abre sesion ninguna.
  const presenceWaiters = new Map(); // id -> [resolve, ...]
  function checkPresence(id) {
    id = String(id || "").replace(/\D/g, "");
    return new Promise((resolve) => {
      if (!ws || !wsReady || !id) { resolve(false); return; }
      const list = presenceWaiters.get(id) || [];
      list.push(resolve);
      presenceWaiters.set(id, list);
      send({ type: "presence", id });
      setTimeout(() => {
        const l = presenceWaiters.get(id);
        if (l && l.includes(resolve)) {
          resolve(false);
          presenceWaiters.set(id, l.filter((r) => r !== resolve));
        }
      }, 4000);
    });
  }

  // ---- Señalizacion (WebSocket al rendezvous) ------------------------------
  function connectSignaling(id) {
    myId = String(id || "").replace(/\D/g, "");
    const url = rendezvousUrl();
    if (!url || !myId) return;
    try {
      ws = new WebSocket(url);
    } catch (e) {
      scheduleReconnect(id);
      return;
    }
    ws.onopen = () => {
      wsReady = true;
      send({ type: "register", id: myId });
    };
    ws.onclose = () => {
      wsReady = false;
      scheduleReconnect(id);
    };
    ws.onerror = () => {
      try { ws.close(); } catch (_) {}
    };
    ws.onmessage = (ev) => {
      let msg;
      try { msg = JSON.parse(ev.data); } catch (_) { return; }
      handleSignal(msg);
    };
  }

  function scheduleReconnect(id) {
    clearTimeout(reconnectTimer);
    reconnectTimer = setTimeout(() => connectSignaling(id), 3000);
  }

  function send(obj) {
    if (ws && wsReady) ws.send(JSON.stringify(obj));
  }
  function signalTo(to, data) {
    send({ type: "signal", to: String(to).replace(/\D/g, ""), data });
  }

  // ---- RTCPeerConnection con cola de candidatos ICE ----------------------
  // Los candidatos ICE del otro extremo pueden llegar ANTES de que hayamos
  // aplicado su offer/answer. Si se hace addIceCandidate sin remoteDescription,
  // el WebView lanza y el candidato se pierde — y perder el candidato bueno hace
  // que la conexion no se abra (pantalla negra "conectando..." para siempre).
  // Aqui se encolan y se aplican en cuanto hay remoteDescription.
  function makePeer(onCandidate) {
    const pc = new RTCPeerConnection(iceConfig());
    pc._pendingIce = [];
    pc._remoteReady = false;
    pc.onicecandidate = (e) => { if (e.candidate) onCandidate(e.candidate); };
    pc.oniceconnectionstatechange = () => {
      console.log("[rtc] iceConnectionState:", pc.iceConnectionState);
    };
    return pc;
  }
  async function applyRemote(pc, desc) {
    await pc.setRemoteDescription(desc);
    pc._remoteReady = true;
    const pend = pc._pendingIce.splice(0);
    for (const c of pend) {
      try { await pc.addIceCandidate(c); }
      catch (e) { console.warn("[rtc] addIceCandidate (diferido) falló:", e); }
    }
  }
  async function addRemoteIce(pc, cand) {
    if (!pc || !cand) return;
    if (!pc._remoteReady) { pc._pendingIce.push(cand); return; }
    try { await pc.addIceCandidate(cand); }
    catch (e) { console.warn("[rtc] addIceCandidate falló:", e); }
  }

  async function handleSignal(msg) {
    if (msg.type === "peer-offline" && viewCbs) {
      viewCbs.onError && viewCbs.onError("El equipo no está conectado (¿app abierta y con internet?).");
      return;
    }
    if (msg.type === "presence-result") {
      const list = presenceWaiters.get(msg.id);
      if (list) { list.forEach((r) => r(!!msg.online)); presenceWaiters.delete(msg.id); }
      return;
    }
    if (msg.type !== "signal") return;
    const from = msg.from;
    const data = msg.data || {};

    // Este equipo recibe una OFERTA -> actua de host.
    if (data.kind === "offer") {
      await hostAnswer(from, data);
    } else if (data.kind === "answer") {
      if (viewPc) await applyRemote(viewPc, { type: "answer", sdp: data.sdp });
    } else if (data.kind === "ice") {
      const pc = data.role === "viewer" ? hostPc : viewPc;
      await addRemoteIce(pc, data.candidate);
    } else if (data.kind === "rejected" && viewCbs) {
      viewCbs.onError && viewCbs.onError("El otro equipo rechazó la conexión.");
      viewCbs.onClose && viewCbs.onClose();
    }
  }

  // ---- Troceado / reensamblado de frames --------------------------------
  // Paquete: [FRAME_MAGIC:u8][seq:u16 BE][idx:u8][total:u8][reservado:u8][datos...]
  let sendFrameSeq = 0;
  function sendFrameChunked(ch, bytes) {
    if (!ch || ch.readyState !== "open") return;
    const total = Math.ceil(bytes.length / FRAME_CHUNK) || 1;
    if (total > 250) {
      console.warn("[frames] frame demasiado grande (" + bytes.length + " B), saltado");
      return;
    }
    const seq = (sendFrameSeq = (sendFrameSeq + 1) & 0xffff);
    for (let i = 0; i < total; i++) {
      const part = bytes.subarray(i * FRAME_CHUNK, (i + 1) * FRAME_CHUNK);
      const pkt = new Uint8Array(6 + part.length);
      pkt[0] = FRAME_MAGIC;
      pkt[1] = (seq >> 8) & 0xff;
      pkt[2] = seq & 0xff;
      pkt[3] = i & 0xff;
      pkt[4] = total & 0xff;
      pkt[5] = 0;
      pkt.set(part, 6);
      try {
        ch.send(pkt);
      } catch (e) {
        console.error("[frames] send falló (trozo " + i + "/" + total + "):", e);
        return;
      }
    }
  }

  // Estado de reensamblado del visor (un frame a la vez; si empieza uno nuevo
  // antes de completar el anterior, se descarta el incompleto).
  const rx = { seq: -1, parts: null, have: 0, total: 0 };
  function reassembleFrame(raw) {
    const p = raw instanceof Uint8Array ? raw : new Uint8Array(raw);
    if (p.length < 6 || p[0] !== FRAME_MAGIC) return null;
    const seq = (p[1] << 8) | p[2];
    const idx = p[3];
    const total = p[4];
    if (total < 1) return null;
    if (seq !== rx.seq) {
      rx.seq = seq;
      rx.total = total;
      rx.have = 0;
      rx.parts = new Array(total).fill(null);
    }
    if (!rx.parts || idx >= rx.total || rx.parts[idx]) return null;
    rx.parts[idx] = p.subarray(6);
    rx.have++;
    if (rx.have !== rx.total) return null;
    let len = 0;
    for (const c of rx.parts) len += c.length;
    const out = new Uint8Array(len);
    let off = 0;
    for (const c of rx.parts) { out.set(c, off); off += c.length; }
    rx.seq = -1;
    rx.parts = null;
    return out;
  }

  // ---- Rol HOST: responde una oferta y comparte pantalla -------------------
  async function hostAnswer(from, offer) {
    // Solo una sesion entrante a la vez.
    if (hostPc) {
      try { hostPc.close(); } catch (_) {}
      teardownHost();
    }
    const profile = offer.profile || "ultralight";

    // Pide autorizacion al usuario de ESTE equipo antes de compartir nada.
    // Sin handler registrado (no debería pasar) se rechaza por seguridad.
    const approved = approvalHandler ? await approvalHandler(from, profile) : false;
    if (!approved) {
      signalTo(from, { kind: "rejected" });
      return;
    }

    sharingProfile = profile;
    if (hostSessionCb) hostSessionCb(true, from);
    hostPc = makePeer((cand) => signalTo(from, { kind: "ice", role: "host", candidate: cand }));

    hostPc.onconnectionstatechange = () => {
      console.log("[rtc/host] connectionState:", hostPc.connectionState);
      if (["failed", "disconnected", "closed"].includes(hostPc.connectionState)) {
        teardownHost();
      }
    };
    hostPc.ondatachannel = (e) => {
      const ch = e.channel;
      if (ch.label === "frames") {
        hostFrames = ch;
        ch.binaryType = "arraybuffer";
        ch.onopen = () => startHostCapture();
        ch.onclose = () => teardownHost();
      } else if (ch.label === "input") {
        hostInput = ch;
        ch.onmessage = (m) => applyIncomingInput(m.data);
      }
    };

    await applyRemote(hostPc, { type: "offer", sdp: offer.sdp });
    const answer = await hostPc.createAnswer();
    await hostPc.setLocalDescription(answer);
    signalTo(from, { kind: "answer", sdp: answer.sdp });
  }

  async function startHostCapture() {
    // Arranca la captura en el backend; los frames llegan por evento `local-frame`.
    try {
      await invoke("start_sharing", { profile: sharingProfile });
    } catch (e) {
      console.error("[host] start_sharing falló:", e);
    }
    if (!localFrameUnlisten) {
      localFrameUnlisten = await listen("local-frame", (ev) => {
        const p = ev.payload || {};
        if (!hostFrames || hostFrames.readyState !== "open") return;
        if (hostFrames.bufferedAmount > FRAME_BUFFER_LIMIT) return; // backpressure
        const bytes = b64ToBytes(p.jpeg);
        if (bytes) sendFrameChunked(hostFrames, bytes);
      });
    }
  }

  function applyIncomingInput(raw) {
    let ev;
    try { ev = JSON.parse(raw); } catch (_) { return; }
    switch (ev.t) {
      case "move": invoke("input_mouse_move", { x: ev.x, y: ev.y }); break;
      case "btn": invoke("input_mouse_button", { button: ev.button, down: ev.down }); break;
      case "scroll": invoke("input_scroll", { delta: ev.delta }); break;
      case "key": invoke("input_key", { vk: ev.vk || 0, code: ev.code || "", down: ev.down }); break;
      case "text": invoke("input_text", { text: ev.text }); break;
    }
  }

  function teardownHost() {
    const wasActive = !!hostPc;
    try { invoke("stop_sharing"); } catch (_) {}
    if (localFrameUnlisten) { try { localFrameUnlisten(); } catch (_) {} localFrameUnlisten = null; }
    hostFrames = null;
    hostInput = null;
    if (hostPc) { try { hostPc.close(); } catch (_) {} hostPc = null; }
    if (wasActive && hostSessionCb) hostSessionCb(false, null);
  }

  // ---- Rol VISOR: crea la oferta y recibe la pantalla ----------------------
  // cbs = { onFrame(bitmap), onOpen(), onClose(), onError(msg), onMetrics(m), onStatus(msg) }
  // Espera hasta ~4s a que la señalizacion este lista (evita perder la oferta
  // si el usuario conecta nada mas arrancar).
  function waitSignaling(id) {
    if (!ws || (ws.readyState !== WebSocket.OPEN && ws.readyState !== WebSocket.CONNECTING)) {
      connectSignaling(id);
    }
    return new Promise((resolve) => {
      const t0 = Date.now();
      const iv = setInterval(() => {
        if (wsReady || Date.now() - t0 > 4000) {
          clearInterval(iv);
          resolve(wsReady);
        }
      }, 100);
    });
  }

  async function connect(peerId, profile, cbs) {
    if (!isConfigured()) {
      cbs.onError && cbs.onError("Falta configurar el servidor en Ajustes.");
      return;
    }
    viewCbs = cbs;
    rx.seq = -1; rx.parts = null; rx.have = 0; rx.total = 0; // limpia reensamblado previo
    const to = String(peerId).replace(/\D/g, "");

    const ready = await waitSignaling(myId || to);
    if (!ready) {
      cbs.onError && cbs.onError("No se pudo contactar el servidor de señalización.");
      return;
    }
    viewPc = makePeer((cand) => signalTo(to, { kind: "ice", role: "viewer", candidate: cand }));

    let frames = 0, bytes = 0, t0 = Date.now();
    let framesShown = 0;

    viewPc.onconnectionstatechange = () => {
      const st = viewPc.connectionState;
      console.log("[rtc/viewer] connectionState:", st);
      if (st === "connected") cbs.onOpen && cbs.onOpen();
      if (["failed", "disconnected", "closed"].includes(st)) {
        cbs.onClose && cbs.onClose();
      }
    };
    viewPc.addEventListener("iceconnectionstatechange", () => {
      if (viewPc.iceConnectionState === "failed") {
        cbs.onError && cbs.onError(
          "No se pudo abrir la conexión P2P (NAT/firewall lo bloquean). " +
          "Prueba otra red o configura un TURN propio en Ajustes."
        );
      }
    });

    // Canal de video: FIABLE y ORDENADO. Los frames van troceados; con este
    // canal los trozos llegan todos y en orden, asi que el reensamblado nunca
    // se queda a medias. (Antes era no-fiable: cualquier trozo perdido tiraba el
    // frame entero -> pantalla negra por internet.)
    viewFrames = viewPc.createDataChannel("frames", { ordered: true });
    viewFrames.binaryType = "arraybuffer";
    viewFrames.onmessage = async (m) => {
      const jpeg = reassembleFrame(m.data);
      if (!jpeg) return;
      framesShown++;
      frames++; bytes += jpeg.byteLength || 0;
      try {
        const blob = new Blob([jpeg], { type: "image/jpeg" });
        const bitmap = await createImageBitmap(blob);
        cbs.onFrame && cbs.onFrame(bitmap);
      } catch (e) {
        console.warn("[frames] decode falló:", e);
      }
      const dt = Date.now() - t0;
      if (dt >= 500) {
        cbs.onMetrics && cbs.onMetrics({
          fps: (frames * 1000) / dt,
          kbps: (bytes * 8) / dt, // bytes/ms*8 = kbit/s
          latency_ms: 0,
        });
        frames = 0; bytes = 0; t0 = Date.now();
      }
    };

    // Canal de control: fiable y ordenado.
    viewInput = viewPc.createDataChannel("input", { ordered: true });

    const offer = await viewPc.createOffer();
    await viewPc.setLocalDescription(offer);
    signalTo(to, { kind: "offer", sdp: offer.sdp, profile });

    // Perro guardian: si a los 8s no ha entrado ni un frame, avisa (sin cerrar
    // la sesion: puede que aun este negociando ICE por TURN).
    setTimeout(() => {
      if (viewCbs === cbs && framesShown === 0) {
        const ice = viewPc ? viewPc.iceConnectionState : "?";
        const conn = viewPc ? viewPc.connectionState : "?";
        console.warn("[rtc] 8s sin frames. ice=" + ice + " conn=" + conn);
        cbs.onStatus && cbs.onStatus(
          "Conectado, esperando vídeo del equipo remoto… (ICE: " + ice + ")"
        );
      }
    }, 8000);
  }

  function sendInput(ev) {
    if (viewInput && viewInput.readyState === "open") {
      try { viewInput.send(JSON.stringify(ev)); } catch (_) {}
    }
  }

  function disconnect() {
    if (viewPc) { try { viewPc.close(); } catch (_) {} viewPc = null; }
    viewFrames = null; viewInput = null; viewCbs = null;
    rx.seq = -1; rx.parts = null; rx.have = 0; rx.total = 0;
  }

  // ---- utilidades ----------------------------------------------------------
  function b64ToBytes(b64) {
    try {
      const bin = atob(b64);
      const len = bin.length;
      const arr = new Uint8Array(len);
      for (let i = 0; i < len; i++) arr[i] = bin.charCodeAt(i);
      return arr;
    } catch (_) {
      return null;
    }
  }

  return {
    isConfigured,
    setRendezvous,
    rendezvousUrl,
    connectSignaling,
    connect,
    sendInput,
    disconnect,
    setApprovalHandler,
    setHostSessionHandler,
    endHostSession,
    checkPresence,
  };
})();
