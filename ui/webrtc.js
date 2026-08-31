// OtisCorp Remote — capa P2P por internet usando WebRTC (nativo del WebView).
//
// El WebView (Chromium/WebView2) trae un stack WebRTC maduro: hace STUN + hole
// punching + cifrado DTLS por su cuenta. Aqui solo:
//   - hablamos con el servidor rendezvous (señalizacion) por WebSocket,
//   - creamos la RTCPeerConnection y dos data channels:
//       "frames" (no fiable, baja latencia) para el video MJPEG,
//       "input"  (fiable, ordenado) para raton/teclado,
//   - el host reenvia por "frames" los JPEG que emite el backend (evento
//     `local-frame`) y aplica el control recibido llamando a los comandos input_*.
//
// El video/control van DIRECTOS entre las dos PCs; el rendezvous solo intercambia
// el saludo inicial.

window.OtisRTC = (function () {
  "use strict";
  const tauri = window.__TAURI__;
  const invoke = tauri ? tauri.core.invoke : async () => null;
  const listen = tauri ? tauri.event.listen : async () => () => {};

  const ICE = {
    iceServers: [
      { urls: "stun:stun.l.google.com:19302" },
      { urls: "stun:stun1.l.google.com:19302" },
    ],
  };

  const LS_KEY = "otis_rendezvous";
  // Servidor rendezvous propio (Fly.io), preconfigurado de fabrica: el modo
  // internet queda activo desde la primera vez, sin que el usuario tenga que
  // pegar nada en Ajustes.
  const DEFAULT_RENDEZVOUS = "wss://otiscorp-rv-8842.fly.dev";
  const FRAME_BUFFER_LIMIT = 1_000_000; // bytes en cola: si se pasa, descarta frame

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

  async function handleSignal(msg) {
    if (msg.type === "peer-offline" && viewCbs) {
      viewCbs.onError && viewCbs.onError("El equipo no está conectado (¿app abierta y con internet?).");
      return;
    }
    if (msg.type !== "signal") return;
    const from = msg.from;
    const data = msg.data || {};

    // Este equipo recibe una OFERTA -> actua de host.
    if (data.kind === "offer") {
      await hostAnswer(from, data);
    } else if (data.kind === "answer") {
      if (viewPc) await viewPc.setRemoteDescription({ type: "answer", sdp: data.sdp });
    } else if (data.kind === "ice") {
      const pc = data.role === "viewer" ? hostPc : viewPc;
      if (pc && data.candidate) {
        try { await pc.addIceCandidate(data.candidate); } catch (_) {}
      }
    }
  }

  // ---- Rol HOST: responde una oferta y comparte pantalla -------------------
  async function hostAnswer(from, offer) {
    // Solo una sesion entrante a la vez.
    if (hostPc) {
      try { hostPc.close(); } catch (_) {}
      teardownHost();
    }
    sharingProfile = offer.profile || "ultralight";
    hostPc = new RTCPeerConnection(ICE);

    hostPc.onicecandidate = (e) => {
      if (e.candidate) signalTo(from, { kind: "ice", role: "host", candidate: e.candidate });
    };
    hostPc.onconnectionstatechange = () => {
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

    await hostPc.setRemoteDescription({ type: "offer", sdp: offer.sdp });
    const answer = await hostPc.createAnswer();
    await hostPc.setLocalDescription(answer);
    signalTo(from, { kind: "answer", sdp: answer.sdp });
  }

  async function startHostCapture() {
    // Arranca la captura en el backend; los frames llegan por evento `local-frame`.
    try { await invoke("start_sharing", { profile: sharingProfile }); } catch (_) {}
    if (!localFrameUnlisten) {
      localFrameUnlisten = await listen("local-frame", (ev) => {
        const p = ev.payload || {};
        if (!hostFrames || hostFrames.readyState !== "open") return;
        if (hostFrames.bufferedAmount > FRAME_BUFFER_LIMIT) return; // backpressure
        const bytes = b64ToBytes(p.jpeg);
        if (bytes) {
          try { hostFrames.send(bytes); } catch (_) {}
        }
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
    try { invoke("stop_sharing"); } catch (_) {}
    if (localFrameUnlisten) { try { localFrameUnlisten(); } catch (_) {} localFrameUnlisten = null; }
    hostFrames = null;
    hostInput = null;
    if (hostPc) { try { hostPc.close(); } catch (_) {} hostPc = null; }
  }

  // ---- Rol VISOR: crea la oferta y recibe la pantalla ----------------------
  // cbs = { onFrame(bitmap), onOpen(), onClose(), onError(msg), onMetrics(m) }
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
    const to = String(peerId).replace(/\D/g, "");

    const ready = await waitSignaling(myId || to);
    if (!ready) {
      cbs.onError && cbs.onError("No se pudo contactar el servidor de señalización.");
      return;
    }
    viewPc = new RTCPeerConnection(ICE);

    let frames = 0, bytes = 0, t0 = Date.now();

    viewPc.onicecandidate = (e) => {
      if (e.candidate) signalTo(to, { kind: "ice", role: "viewer", candidate: e.candidate });
    };
    viewPc.onconnectionstatechange = () => {
      const st = viewPc.connectionState;
      if (st === "connected") cbs.onOpen && cbs.onOpen();
      if (["failed", "disconnected", "closed"].includes(st)) {
        cbs.onClose && cbs.onClose();
      }
    };

    // Canal de video: no fiable, sin orden -> minima latencia.
    viewFrames = viewPc.createDataChannel("frames", { ordered: false, maxRetransmits: 0 });
    viewFrames.binaryType = "arraybuffer";
    viewFrames.onmessage = async (m) => {
      frames++; bytes += m.data.byteLength || 0;
      try {
        const blob = new Blob([m.data], { type: "image/jpeg" });
        const bitmap = await createImageBitmap(blob);
        cbs.onFrame && cbs.onFrame(bitmap);
      } catch (_) {}
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
  }

  function sendInput(ev) {
    if (viewInput && viewInput.readyState === "open") {
      try { viewInput.send(JSON.stringify(ev)); } catch (_) {}
    }
  }

  function disconnect() {
    if (viewPc) { try { viewPc.close(); } catch (_) {} viewPc = null; }
    viewFrames = null; viewInput = null; viewCbs = null;
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
  };
})();
