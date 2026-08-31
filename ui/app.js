// OtisCorp Remote — frontend de la pantalla principal.
// Arranca directo: pide el bootstrap al backend (ID auto-generado + perfil real)
// y refresca las metricas del cliente por polling. Sin codigo de activacion.

(function () {
  "use strict";

  // API de Tauri (withGlobalTauri). Fallback a no-op para previsualizar en navegador.
  const tauri = window.__TAURI__;
  const invoke = tauri ? tauri.core.invoke : async () => null;
  const listen = tauri ? tauri.event.listen : async () => () => {};
  const currentWindow = tauri ? tauri.window.getCurrentWindow() : null;
  const updaterApi = tauri ? tauri.updater : null;
  const processApi = tauri ? tauri.process : null;

  const $ = (id) => document.getElementById(id);

  // ---- Formateo -----------------------------------------------------------
  function groupId(raw) {
    // "731204998" -> "731 204 998"; agrupa de 3 en 3.
    const s = String(raw || "").replace(/\D/g, "");
    return s.replace(/(\d{3})(?=\d)/g, "$1 ").trim();
  }
  function spacePw(raw) {
    return String(raw || "").split("").join(" ");
  }
  const PROFILE_LABEL = { ultralight: "Ultraligero", balanced: "Equilibrado", sharp: "Nítido" };

  // ---- Toast no bloqueante ------------------------------------------------
  let toastTimer = null;
  function toast(msg) {
    const el = $("toast");
    el.textContent = msg;
    el.classList.add("show");
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => el.classList.remove("show"), 1600);
  }

  // ---- Autorizacion de conexiones entrantes --------------------------------
  // Se usa tanto para LAN como para P2P: muestra el dialogo, cuenta 19s y
  // devuelve una Promise<boolean> con la decision (o rechazo por timeout).
  const approvalBackdrop = $("approval-backdrop");
  const approvalText = $("approval-text");
  const approvalCountdown = $("approval-countdown");
  let approvalResolve = null;
  let approvalTimer = null;

  function requestApproval(peerLabel, profile) {
    return new Promise((resolve) => {
      // Si ya habia una solicitud pendiente (no deberia pasar, solo 1 a la
      // vez), la resolvemos como rechazada antes de mostrar la nueva.
      if (approvalResolve) { approvalResolve(false); }
      approvalResolve = resolve;
      approvalText.textContent =
        `El equipo ${peerLabel} quiere conectarse y controlar esta pantalla` +
        (profile ? ` (perfil ${PROFILE_LABEL[profile] || profile}).` : ".");
      // 45s (antes 19): da tiempo a llegar al otro equipo y pulsar Autorizar
      // cuando pruebas tu solo con dos maquinas.
      let secs = 45;
      approvalCountdown.textContent = String(secs);
      approvalBackdrop.classList.remove("hidden");
      clearInterval(approvalTimer);
      approvalTimer = setInterval(() => {
        secs -= 1;
        approvalCountdown.textContent = String(Math.max(secs, 0));
        if (secs <= 0) finishApproval(false);
      }, 1000);
    });
  }
  function finishApproval(accept) {
    clearInterval(approvalTimer);
    approvalBackdrop.classList.add("hidden");
    if (approvalResolve) { approvalResolve(accept); approvalResolve = null; }
  }
  $("approval-accept").addEventListener("click", () => finishApproval(true));
  $("approval-reject").addEventListener("click", () => finishApproval(false));

  // Camino P2P: webrtc.js pide la decision antes de contestar la oferta.
  if (window.OtisRTC) {
    OtisRTC.setApprovalHandler((peerId, profile) => requestApproval(groupId(peerId), profile));
  }

  // Camino LAN: el backend emite el evento y espera el comando de vuelta.
  listen("incoming-request", (e) => {
    const { peer, profile } = e.payload || {};
    requestApproval(peer || "desconocido", profile).then((accept) => {
      invoke("respond_incoming", { accept });
    });
  });

  // ---- Barra "te estan controlando" (lado host, ambos transportes) --------
  const hostBar = $("host-bar");
  const hostBarText = $("host-bar-text");
  let hostBarMode = null; // "lan" | "rtc"
  function showHostBar(mode, peer) {
    hostBarMode = mode;
    hostBarText.textContent = peer ? `Te está controlando ${groupId(peer)}` : "Te están controlando";
    hostBar.classList.remove("hidden");
  }
  function hideHostBar() {
    hostBarMode = null;
    hostBar.classList.add("hidden");
  }
  listen("incoming-session-started", (e) => showHostBar("lan", (e.payload || {}).peer));
  listen("incoming-session-ended", () => { if (hostBarMode === "lan") hideHostBar(); });
  if (window.OtisRTC) {
    OtisRTC.setHostSessionHandler((active, peer) => {
      if (active) showHostBar("rtc", peer); else if (hostBarMode === "rtc") hideHostBar();
    });
  }
  $("host-bar-end").addEventListener("click", () => {
    if (hostBarMode === "lan") invoke("end_incoming_session");
    else if (hostBarMode === "rtc" && window.OtisRTC) OtisRTC.endHostSession();
    hideHostBar();
  });

  // ---- Auto-actualizacion (GitHub Releases) --------------------------------
  // Revisa al arrancar; si hay version nueva, la descarga e instala sola y
  // reinicia la app. No requiere accion del usuario.
  async function checkForUpdate() {
    if (!updaterApi || !processApi) return;
    try {
      const update = await updaterApi.check();
      if (!update) return;
      $("update-text").textContent = `Actualizando a la versión ${update.version}…`;
      $("update-overlay").classList.remove("hidden");
      await update.downloadAndInstall();
      await processApi.relaunch();
    } catch (_) {
      // Sin internet o sin releases publicados aun: seguimos con la version actual.
      $("update-overlay").classList.add("hidden");
    }
  }

  // ---- Estado -------------------------------------------------------------
  let sessionPassword = "";
  let myDeviceId = "";

  // ---- Bootstrap ----------------------------------------------------------
  async function bootstrap() {
    let data;
    try {
      data = await invoke("bootstrap");
    } catch (e) {
      $("diag-text").textContent = "No se pudo leer el perfil del equipo.";
      return;
    }
    if (!data) {
      // Modo previsualizacion en navegador (sin backend Tauri).
      data = {
        id: "731204998",
        device_name: "Mi PC",
        unattended: true,
        session_password: "7429",
        profile: {
          hostname: "Mi PC",
          os_name: "Windows",
          cpu_brand: "CPU (preview)",
          cpu_cores: 4,
          total_ram_mb: 4096,
          is_low_end: true,
          default_profile: "ultralight",
        },
      };
    }

    sessionPassword = data.session_password;
    myDeviceId = data.id;
    $("my-id").textContent = groupId(data.id);
    $("my-pw").textContent = spacePw(sessionPassword);

    // Si hay servidor rendezvous configurado, conecta la señalizacion para poder
    // RECIBIR conexiones por internet (acceso desatendido remoto).
    if (window.OtisRTC && OtisRTC.isConfigured()) {
      OtisRTC.connectSignaling(data.id);
    }
    updateNetModeLabel();

    $("unattended-tag").classList.toggle("hidden", !data.unattended);
    $("device-line").textContent = `${data.device_name} · ${data.profile.os_name}`;

    // Franja de diagnostico con datos reales.
    const p = data.profile;
    const ramGb = (p.total_ram_mb / 1024).toFixed(p.total_ram_mb < 8192 ? 1 : 0);
    if (p.is_low_end) {
      $("diag-text").innerHTML =
        `Este PC es gama baja (${escapeHtml(shortCpu(p.cpu_brand))}, ${ramGb} GB) — perfil ` +
        `<strong>Ultraligero</strong> aplicado por defecto`;
    } else {
      $("diag-text").innerHTML =
        `${escapeHtml(shortCpu(p.cpu_brand))} · ${ramGb} GB · ${p.cpu_cores} núcleos — perfil ` +
        `<strong>${PROFILE_LABEL[p.default_profile] || "Equilibrado"}</strong> por defecto`;
    }

    // Selecciona el perfil por defecto en el segmented control de "Conectar a".
    selectProfile(p.default_profile);
  }

  function shortCpu(brand) {
    // Recorta "Intel(R) Celeron(R) N4020 CPU @ 1.10GHz" -> "Celeron N4020".
    if (!brand) return "CPU";
    let b = brand
      .replace(/\(R\)|\(TM\)|CPU|Processor|@.*$/gi, "")
      .replace(/Intel|AMD|Genuine/gi, "")
      .replace(/\s+/g, " ")
      .trim();
    return b || brand;
  }
  function escapeHtml(s) {
    return String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
  }

  // ---- Metricas en vivo ---------------------------------------------------
  async function refreshMetrics() {
    try {
      const m = await invoke("client_metrics");
      if (m) {
        $("client-metrics").textContent =
          `Cliente: ${m.client_ram_mb} MB RAM · ${m.client_cpu_pct.toFixed(1)} % CPU`;
      }
    } catch (_) {
      /* sin backend en preview */
    }
  }

  // ---- Perfil de calidad --------------------------------------------------
  function selectProfile(value) {
    document.querySelectorAll("#profile-seg .seg-opt").forEach((opt) => {
      const input = opt.querySelector("input");
      const on = input.value === value;
      input.checked = on;
      opt.classList.toggle("on", on);
    });
  }
  document.querySelectorAll("#profile-seg .seg-opt").forEach((opt) => {
    opt.addEventListener("click", () => selectProfile(opt.querySelector("input").value));
  });

  // ---- Pestanas -----------------------------------------------------------
  function showTab(name) {
    document.querySelectorAll(".tab").forEach((t) =>
      t.classList.toggle("on", t.dataset.tab === name)
    );
    document.querySelectorAll(".tabpage").forEach((pg) =>
      pg.classList.toggle("hidden", pg.dataset.page !== name)
    );
  }
  document.querySelectorAll("[data-tab]").forEach((el) => {
    el.addEventListener("click", () => showTab(el.dataset.tab));
  });

  // ---- Acciones del puesto ------------------------------------------------
  $("btn-copy").addEventListener("click", async () => {
    const text = $("my-id").textContent.replace(/\s/g, "");
    try {
      await navigator.clipboard.writeText(text);
      toast("ID copiado al portapapeles");
    } catch (_) {
      toast("No se pudo copiar");
    }
  });

  $("btn-regen").addEventListener("click", async () => {
    try {
      const pw = await invoke("regenerate_password");
      if (pw) {
        sessionPassword = pw;
        $("my-pw").textContent = spacePw(pw);
        toast("Contraseña de sesión regenerada");
      }
    } catch (_) {
      toast("No disponible en previsualización");
    }
  });

  function currentProfile() {
    const sel = document.querySelector('#profile-seg input:checked');
    return sel ? sel.value : "ultralight";
  }

  $("btn-connect").addEventListener("click", () => {
    startConnect($("peer-id").value, currentProfile());
  });

  // Punto de entrada unico de conexion (lo usan el boton y los equipos guardados).
  // Acepta un ID de 9 digitos O una IP local (192.168.x.x): con IP la conexion
  // va SIEMPRE directa por la LAN — sin descubrimiento, sin relay, latencia
  // minima. Es la opcion mas fluida si los dos equipos estan en la misma red.
  function startConnect(rawId, profile) {
    const raw = String(rawId).trim();
    const isIp = /^(\d{1,3}\.){3}\d{1,3}$/.test(raw);
    const peer = isIp ? raw : raw.replace(/\D/g, "");
    if (!isIp && peer.length < 9) {
      toast("Introduce un ID de 9 dígitos (o una IP local: 192.168.x.x)");
      return;
    }
    profile = profile || currentProfile();
    if (!isIp) {
      // Guarda/actualiza el equipo para la proxima (como AnyDesk).
      Devices.upsert(peer, null, profile);
      Devices.renderAll();
    }
    $("conn-status").textContent = `Conectando a ${isIp ? peer : groupId(peer)}…`;
    $("conn-loader").classList.remove("hidden");
    if (isIp) { connectViaLan(peer, profile); return; } // IP local => directo, sin relay
    // Transporte por defecto: NATIVO del backend — descubrimiento LAN si el
    // equipo esta en la misma red, o tunel TCP por el relay
    // (otiscorp-relay.fly.dev:443, TLS saliente) si no. Cruza cualquier
    // NAT/firewall sin STUN ni TURN ni hole-punching; es TCP fiable y ordenado.
    // WebRTC P2P queda como opcion avanzada (localStorage otis_force_webrtc="1").
    let forceRtc = false;
    try { forceRtc = localStorage.getItem("otis_force_webrtc") === "1"; } catch (_) {}
    if (forceRtc && window.OtisRTC && OtisRTC.isConfigured()) {
      connectViaRTC(peer, profile);
    } else {
      connectViaLan(peer, profile);
    }
  }

  // Conexion por LAN (transporte TCP del backend).
  async function connectViaLan(peer, profile) {
    const unlisteners = [];
    const driver = {
      sendInput: (ev) => invoke("send_remote_input", { ev }),
      close: () => {
        unlisteners.forEach((u) => { try { u(); } catch (_) {} });
        try { invoke("disconnect_peer"); } catch (_) {}
      },
    };
    const label = /[.:]/.test(peer) ? peer : groupId(peer);
    RemoteSession.open("Sesión · " + label, profile, driver, peer);
    // Camino H.264 (el normal: hardware/software segun el equipo host).
    unlisteners.push(await listen("remote-frame-h264", (e) => {
      const p = e.payload || {};
      if (p.data) RemoteSession.drawH264(p.data, p.width, p.height, !!p.keyframe);
    }));
    // Camino MJPEG frame completo (Ultraligero, o fallback).
    unlisteners.push(await listen("remote-frame", (e) => {
      const p = e.payload || {};
      if (p.jpeg) RemoteSession.drawJpegB64(p.jpeg, p.width, p.height);
    }));
    // Camino JPEG por celdas (Nítido / Equilibrado): solo trozos que cambiaron.
    unlisteners.push(await listen("remote-frame-tiles", (e) => {
      const p = e.payload || {};
      if (p.data) RemoteSession.drawJpegTiles(p.data, p.width, p.height, !!p.keyframe);
    }));
    unlisteners.push(await listen("remote-metrics", (e) => RemoteSession.setMetrics(e.payload || {})));
    // El backend avisa cuando la sesion termina (host rechazo, relay/peer cerro…).
    // Solo actuamos si la sesion seguia viva: al pulsar "Terminar" ya se cierra
    // sola y este evento llega despues (evita un toast redundante).
    unlisteners.push(await listen("remote-ended", (e) => {
      if (!RemoteSession.isActive()) return;
      const reason = (e.payload || {}).reason || "";
      toast(reason === "rejected"
        ? "El otro equipo rechazó la conexión."
        : "Sesión finalizada" + (reason ? " (" + reason + ")" : "") + ".");
      $("conn-loader").classList.add("hidden");
      RemoteSession.close();
    }));
    try {
      $("conn-status").textContent = `Conectando a ${groupId(peer)}… (LAN o relay)`;
      await invoke("connect_peer", { peerId: peer, profile });
      $("conn-status").textContent = "Conectado · esperando autorización del otro equipo…";
      $("conn-loader").classList.add("hidden");
    } catch (e) {
      toast("No se pudo conectar: " + e);
      $("conn-loader").classList.add("hidden");
      RemoteSession.close();
    }
  }

  // Conexion por internet (WebRTC P2P vía rendezvous).
  function connectViaRTC(peer, profile) {
    const driver = {
      sendInput: (ev) => OtisRTC.sendInput(ev),
      close: () => OtisRTC.disconnect(),
    };
    RemoteSession.open("Sesión · " + groupId(peer), profile, driver, peer);
    OtisRTC.connect(peer, profile, {
      onFrame: (bmp) => RemoteSession.drawBitmap(bmp),
      onMetrics: (m) => RemoteSession.setMetrics(m),
      onOpen: () => { $("conn-status").textContent = "Conectado (P2P)"; $("conn-loader").classList.add("hidden"); },
      onClose: () => { $("conn-loader").classList.add("hidden"); RemoteSession.close(); },
      onError: (msg) => { $("conn-loader").classList.add("hidden"); toast(msg); RemoteSession.close(); },
      // Aviso no fatal (p. ej. "conectado pero sin vídeo aún"): informa sin
      // cerrar la sesión, que puede estar aún negociando ICE por el TURN.
      onStatus: (msg) => { $("conn-status").textContent = msg; toast(msg); },
    });
  }

  // ---- Vista de sesion remota (visor), agnostica al transporte -------------
  // El "driver" (LAN o WebRTC) provee sendInput(ev) y close(); esta vista se
  // encarga de pintar los frames y capturar raton/teclado.
  const RemoteSession = (function () {
    const view = $("remote-view");
    const canvas = $("remote-canvas");
    const ctx = canvas.getContext("2d", { alpha: false });
    const img = new Image();
    let active = false, controlOn = true, startTs = 0, timerId = null;
    let driver = null;
    let sessionPeerId = "";

    // ---- Decodificador H.264 (WebCodecs) para el camino LAN/internet por TCP.
    // El backend manda Annex B (start codes, SPS/PPS delante de cada keyframe);
    // WebCodecs lo entiende directo con `avc: { format: "annexb" }` sin que
    // tengamos que convertir a AVCC en JS.
    let h264Decoder = null;
    let h264WaitingKeyframe = true;
    let h264W = 0, h264H = 0;

    function b64ToBytes(b64) {
      const bin = atob(b64);
      const arr = new Uint8Array(bin.length);
      for (let i = 0; i < bin.length; i++) arr[i] = bin.charCodeAt(i);
      return arr;
    }

    function closeH264Decoder() {
      if (h264Decoder) {
        try { h264Decoder.close(); } catch (_) {}
        h264Decoder = null;
      }
      h264WaitingKeyframe = true;
      h264W = 0; h264H = 0;
    }

    function ensureH264Decoder() {
      if (h264Decoder) return;
      h264Decoder = new VideoDecoder({
        output: (frame) => {
          if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
            canvas.width = frame.displayWidth;
            canvas.height = frame.displayHeight;
          }
          ctx.drawImage(frame, 0, 0, canvas.width, canvas.height);
          frame.close();
        },
        error: (e) => {
          console.error("[h264] error del decoder:", e);
          closeH264Decoder();
          invoke("request_remote_keyframe").catch(() => {});
        },
      });
      h264Decoder.configure({ codec: "avc1.42E01E", avc: { format: "annexb" }, optimizeForLatency: true });
      h264WaitingKeyframe = true;
    }

    // Pinta un frame H.264 (Annex B en base64) del camino LAN/internet TCP.
    function drawH264(b64, w, h, keyframe) {
      if (!active) return;
      if (typeof VideoDecoder === "undefined") {
        // WebView2 demasiado antiguo para WebCodecs: no hay como decodificar.
        return;
      }
      if (w !== h264W || h !== h264H) {
        closeH264Decoder();
        h264W = w; h264H = h;
      }
      ensureH264Decoder();
      if (h264WaitingKeyframe && !keyframe) {
        // Nos perdimos la keyframe (reconexion, resize): pedimos otra y
        // descartamos este delta, que el decoder no puede usar sin ella.
        invoke("request_remote_keyframe").catch(() => {});
        return;
      }
      h264WaitingKeyframe = false;
      try {
        h264Decoder.decode(
          new EncodedVideoChunk({
            type: keyframe ? "key" : "delta",
            timestamp: performance.now() * 1000,
            data: b64ToBytes(b64),
          })
        );
      } catch (e) {
        console.error("[h264] decode fallo:", e);
        closeH264Decoder();
        invoke("request_remote_keyframe").catch(() => {});
      }
    }

    function fmtTime(ms) {
      const s = Math.floor(ms / 1000);
      return String(Math.floor(s / 60)).padStart(2, "0") + ":" + String(s % 60).padStart(2, "0");
    }

    function open(title, profile, drv, peerId) {
      driver = drv;
      sessionPeerId = peerId || "";
      active = true; controlOn = true;
      tilesInFlight = false; tilesPending = null; jpegDecoding = false;
      view.classList.remove("hidden");
      $("session-title").textContent = title;
      $("sb-quality").textContent = PROFILE_LABEL[profile] || "Ultraligero";
      $("sb-input").textContent = "Control: on";
      startTs = Date.now();
      clearInterval(timerId);
      timerId = setInterval(() => {
        $("session-timer").textContent = fmtTime(Date.now() - startTs);
      }, 500);
      // El foco del canvas no basta si la ventana del SO no esta enfocada
      // (pasa al abrir la sesion recien conectado): forzamos foco de ventana
      // y reintentamos el foco del canvas tras el reflow.
      if (currentWindow) { try { currentWindow.setFocus(); } catch (_) {} }
      requestAnimationFrame(() => canvas.focus());
    }

    // Pinta un ImageBitmap (camino WebRTC).
    function drawBitmap(bitmap) {
      if (!active) return;
      if (canvas.width !== bitmap.width || canvas.height !== bitmap.height) {
        canvas.width = bitmap.width; canvas.height = bitmap.height;
      }
      ctx.drawImage(bitmap, 0, 0);
      if (bitmap.close) bitmap.close();
    }

    // Pinta un JPEG en base64 (camino LAN/relay). Decodifica con
    // createImageBitmap (fuera del hilo principal, mas rapido que un
    // data: URL) y SUELTA frames si el decode va por detras: mejor ver
    // el ultimo frame que acumular retraso.
    let jpegDecoding = false;
    async function drawJpegB64(b64, w, h) {
      if (!active || jpegDecoding) return;
      jpegDecoding = true;
      try {
        const bin = atob(b64);
        const bytes = new Uint8Array(bin.length);
        for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
        const bmp = await createImageBitmap(new Blob([bytes], { type: "image/jpeg" }));
        if (active) {
          if (canvas.width !== bmp.width || canvas.height !== bmp.height) {
            canvas.width = bmp.width; canvas.height = bmp.height;
          }
          ctx.drawImage(bmp, 0, 0);
        }
        if (bmp.close) bmp.close();
      } catch (_) {
        // frame corrupto: lo saltamos, el siguiente repinta
      }
      jpegDecoding = false;
    }

    // Pinta un frame por CELDAS (CODEC_JPEG_TILES, perfiles Nítido/Equilibrado).
    // El lienzo es PERSISTENTE: solo se repintan los trozos que cambiaron. En un
    // keyframe llega una sola celda con la pantalla entera. Los frames de celdas
    // son incrementales -> no se descartan; si uno llega mientras se pinta el
    // anterior, se guarda el ÚLTIMO y se procesa al terminar.
    let tilesInFlight = false, tilesPending = null;
    function drawJpegTiles(b64, fullW, fullH, keyframe) {
      if (!active) return;
      if (tilesInFlight) { tilesPending = [b64, fullW, fullH, keyframe]; return; }
      tilesInFlight = true;
      paintTiles(b64, fullW, fullH, keyframe).catch(() => {}).then(() => {
        tilesInFlight = false;
        const p = tilesPending;
        if (p) { tilesPending = null; drawJpegTiles(p[0], p[1], p[2], p[3]); }
      });
    }
    async function paintTiles(b64, fullW, fullH, keyframe) {
      const bin = atob(b64);
      const u8 = new Uint8Array(bin.length);
      for (let i = 0; i < bin.length; i++) u8[i] = bin.charCodeAt(i);
      const dv = new DataView(u8.buffer);
      let off = 0;
      const n = dv.getUint16(off); off += 2;
      const jobs = [];
      for (let i = 0; i < n; i++) {
        const x = dv.getUint16(off); off += 2;
        const y = dv.getUint16(off); off += 2;
        dv.getUint16(off); off += 2; // w (implícito en el bitmap)
        dv.getUint16(off); off += 2; // h
        const len = dv.getUint32(off); off += 4;
        const jpg = u8.subarray(off, off + len); off += len;
        jobs.push(createImageBitmap(new Blob([jpg], { type: "image/jpeg" })).then(
          (bmp) => ({ bmp, x, y }), () => null
        ));
      }
      const parts = await Promise.all(jobs);
      if (!active) { parts.forEach((p) => p && p.bmp.close && p.bmp.close()); return; }
      if (keyframe && fullW && fullH && (canvas.width !== fullW || canvas.height !== fullH)) {
        canvas.width = fullW; canvas.height = fullH;
      }
      for (const p of parts) {
        if (!p) continue;
        ctx.drawImage(p.bmp, p.x, p.y);
        if (p.bmp.close) p.bmp.close();
      }
    }

    function setMetrics(m) {
      $("session-metrics").textContent =
        `${Math.round(m.fps || 0)} fps · ${Math.round(m.latency_ms || 0)} ms · ${Math.round(m.kbps || 0)} kb/s`;
    }

    // Coordenadas normalizadas 0..1 respecto al contenido REAL del canvas.
    // El canvas se escala con object-fit:contain para llenar la ventana sin
    // deformar la imagen; si la proporcion no coincide exactamente con la del
    // equipo remoto quedan franjas dentro de la misma caja, y hay que
    // descontarlas o el mouse se desvia (precision perdida, sobre todo cerca
    // de los bordes).
    function norm(e) {
      const r = canvas.getBoundingClientRect();
      const iw = canvas.width, ih = canvas.height;
      if (!iw || !ih || !r.width || !r.height) return { x: 0, y: 0 };
      const imgAspect = iw / ih, boxAspect = r.width / r.height;
      let renderW, renderH, offX, offY;
      if (imgAspect > boxAspect) {
        renderW = r.width; renderH = r.width / imgAspect;
        offX = 0; offY = (r.height - renderH) / 2;
      } else {
        renderH = r.height; renderW = r.height * imgAspect;
        offY = 0; offX = (r.width - renderW) / 2;
      }
      const x = (e.clientX - r.left - offX) / renderW;
      const y = (e.clientY - r.top - offY) / renderH;
      return { x: Math.min(1, Math.max(0, x)), y: Math.min(1, Math.max(0, y)) };
    }
    function send(evt) {
      if (!controlOn || !active || !driver) return;
      try { driver.sendInput(evt); } catch (_) {}
    }

    const BTN = { 0: "left", 1: "middle", 2: "right" };
    // El raton dispara 100-200 mousemove/s; enviarlos todos satura el canal de
    // entrada y añade lag. Se coalescen: se manda YA el primero y luego, como
    // mucho, uno cada 12 ms con la ultima posicion (trailing).
    let lastMoveSent = 0, pendingMove = null, moveFlushTimer = null;
    function flushMove() {
      moveFlushTimer = null;
      if (!pendingMove) return;
      send(pendingMove); pendingMove = null; lastMoveSent = performance.now();
    }
    function queueMove(n) {
      const now = performance.now();
      if (now - lastMoveSent >= 12) {
        send({ t: "move", x: n.x, y: n.y }); lastMoveSent = now; pendingMove = null;
      } else {
        pendingMove = { t: "move", x: n.x, y: n.y };
        if (!moveFlushTimer) moveFlushTimer = setTimeout(flushMove, 12);
      }
    }
    canvas.addEventListener("mousemove", (e) => queueMove(norm(e)));
    canvas.addEventListener("mousedown", (e) => {
      e.preventDefault(); canvas.focus();
      // Clic: posicion exacta AHORA (sin throttle) y luego el boton.
      const n = norm(e);
      if (moveFlushTimer) { clearTimeout(moveFlushTimer); moveFlushTimer = null; }
      pendingMove = null;
      send({ t: "move", x: n.x, y: n.y }); lastMoveSent = performance.now();
      send({ t: "btn", button: BTN[e.button] || "left", down: true });
    });
    canvas.addEventListener("mouseup", (e) => {
      e.preventDefault(); send({ t: "btn", button: BTN[e.button] || "left", down: false });
    });
    canvas.addEventListener("contextmenu", (e) => e.preventDefault());
    canvas.addEventListener("wheel", (e) => {
      e.preventDefault(); send({ t: "scroll", delta: e.deltaY < 0 ? 120 : -120 });
    }, { passive: false });
    function onKeyDown(e) {
      if (!active) return;
      e.preventDefault(); send({ t: "key", vk: vkFromEvent(e), code: e.code || "", down: true });
    }
    function onKeyUp(e) {
      if (!active) return;
      e.preventDefault(); send({ t: "key", vk: vkFromEvent(e), code: e.code || "", down: false });
    }
    // Escuchamos a nivel de ventana (no solo canvas): el foco del elemento no
    // basta si la ventana del SO no esta activa, y evitamos doble disparo por
    // burbujeo si el listener estuviera tambien en el canvas.
    window.addEventListener("keydown", onKeyDown);
    window.addEventListener("keyup", onKeyUp);
    view.addEventListener("mousedown", () => canvas.focus());

    // Guarda una miniatura (240px de ancho) del ultimo frame visto, para la
    // tarjeta del dispositivo en la libreta. No es vista en vivo: solo queda
    // el ultimo estado al cerrar la sesion.
    function saveThumbnail() {
      if (!sessionPeerId || !canvas.width || !canvas.height) return;
      try {
        const thumbW = 240;
        const thumbH = Math.round((canvas.height / canvas.width) * thumbW) || 135;
        const t = document.createElement("canvas");
        t.width = thumbW; t.height = thumbH;
        t.getContext("2d").drawImage(canvas, 0, 0, thumbW, thumbH);
        Devices.saveThumbnail(sessionPeerId, t.toDataURL("image/jpeg", 0.6));
      } catch (_) { /* canvas vacio o tainted: sin miniatura, no es grave */ }
    }

    function close() {
      if (!active && view.classList.contains("hidden")) return;
      saveThumbnail();
      active = false;
      clearInterval(timerId);
      closeH264Decoder();
      if (driver) { try { driver.close(); } catch (_) {} driver = null; }
      view.classList.add("hidden");
      $("conn-status").textContent = "En línea · listo";
      if (currentWindow) { currentWindow.setFullscreen(false).catch(() => {}); }
    }

    $("sb-end").addEventListener("click", close);
    $("sb-input").addEventListener("click", () => {
      controlOn = !controlOn;
      $("sb-input").textContent = "Control: " + (controlOn ? "on" : "off");
    });

    // Pantalla completa real (a nivel de SO), como AnyDesk. Se sincroniza si
    // el usuario sale con Esc (el navegador/OS dispara fullscreenchange).
    let isFullscreen = false;
    async function setFullscreen(on) {
      if (!currentWindow) return;
      try { await currentWindow.setFullscreen(on); isFullscreen = on; } catch (_) {}
      $("sb-fullscreen").textContent = on ? "Salir de pantalla completa" : "Pantalla completa";
    }
    $("sb-fullscreen").addEventListener("click", () => setFullscreen(!isFullscreen));
    if (currentWindow) {
      currentWindow.onResized(() => {
        if (active) currentWindow.isFullscreen().then((v) => {
          isFullscreen = v;
          $("sb-fullscreen").textContent = v ? "Salir de pantalla completa" : "Pantalla completa";
        }).catch(() => {});
      });
    }

    // Captura: guarda el frame actual del escritorio remoto como PNG.
    $("sb-screenshot").addEventListener("click", () => {
      try {
        const a = document.createElement("a");
        a.href = canvas.toDataURL("image/png");
        a.download = `otiscorp-captura-${Date.now()}.png`;
        a.click();
        toast("Captura guardada");
      } catch (_) {
        toast("No se pudo guardar la captura");
      }
    });

    // Ctrl+Alt+Supr en el equipo remoto (secuencia keydown en orden, luego keyup en reversa).
    $("sb-cad").addEventListener("click", () => {
      if (!controlOn || !active || !driver) return;
      const seq = [
        { vk: 0xA2, code: "ControlLeft" },
        { vk: 0xA4, code: "AltLeft" },
        { vk: 0x2E, code: "Delete" },
      ];
      seq.forEach((k) => send({ t: "key", vk: k.vk, code: k.code, down: true }));
      seq.slice().reverse().forEach((k) => send({ t: "key", vk: k.vk, code: k.code, down: false }));
      toast("Ctrl+Alt+Supr enviado");
    });

    // Controles de la ventana durante la sesion (el titlebar normal queda
    // tapado por la vista de sesion a pantalla completa).
    if (currentWindow) {
      $("sb-win-min").addEventListener("click", () => currentWindow.minimize());
      $("sb-win-max").addEventListener("click", () => currentWindow.toggleMaximize());
      $("sb-win-close").addEventListener("click", () => currentWindow.close());
    }

    return { open, close, drawBitmap, drawJpegB64, drawJpegTiles, drawH264, setMetrics, isActive: () => active };
  })();

  // Mapea un KeyboardEvent del navegador a un codigo de tecla virtual de Windows.
  // Usa e.code para las teclas estables y cae a e.keyCode (que en la practica
  // coincide con los VK de Windows para letras, digitos y F1..F12).
  function vkFromEvent(e) {
    const code = e.code || "";
    if (/^Key[A-Z]$/.test(code)) return code.charCodeAt(3); // A..Z -> 0x41..0x5A
    if (/^Digit[0-9]$/.test(code)) return code.charCodeAt(5); // 0..9 -> 0x30..0x39
    if (/^F([1-9]|1[0-9]|2[0-4])$/.test(code)) return 0x70 + (parseInt(code.slice(1), 10) - 1);
    const MAP = {
      Enter: 0x0D, Escape: 0x1B, Backspace: 0x08, Tab: 0x09, Space: 0x20,
      ArrowLeft: 0x25, ArrowUp: 0x26, ArrowRight: 0x27, ArrowDown: 0x28,
      Delete: 0x2E, Home: 0x24, End: 0x23, PageUp: 0x21, PageDown: 0x22, Insert: 0x2D,
      ShiftLeft: 0xA0, ShiftRight: 0xA1, ControlLeft: 0xA2, ControlRight: 0xA3,
      AltLeft: 0xA4, AltRight: 0xA5, MetaLeft: 0x5B, MetaRight: 0x5C,
      CapsLock: 0x14,
      Minus: 0xBD, Equal: 0xBB, BracketLeft: 0xDB, BracketRight: 0xDD, Backslash: 0xDC,
      Semicolon: 0xBA, Quote: 0xDE, Backquote: 0xC0, Comma: 0xBC, Period: 0xBE, Slash: 0xBF,
    };
    if (MAP[code] != null) return MAP[code];
    return e.keyCode || 0;
  }

  // ---- Motor de captura (prueba local del engine DXGI) --------------------
  // Boton para arrancar/parar la captura del propio escritorio y ver metricas
  // reales (fps, resolucion, throughput) mientras el transporte no existe.
  let capturing = false;
  const btnCapture = $("btn-capture");
  if (btnCapture) {
    btnCapture.addEventListener("click", async () => {
      try {
        if (!capturing) {
          await invoke("start_capture");
          capturing = true;
          btnCapture.textContent = "Detener captura";
          $("conn-status").textContent = "Capturando escritorio local…";
        } else {
          await invoke("stop_capture");
          capturing = false;
          btnCapture.textContent = "Probar captura";
          $("conn-status").textContent = "En línea · listo";
          $("diag-net").textContent = "";
        }
      } catch (e) {
        toast("Captura no disponible: " + e);
      }
    });
  }

  // Escucha las estadisticas que emite el hilo de captura (~2/seg).
  listen("capture-stats", (event) => {
    const s = event.payload || {};
    if (!s.running) return;
    $("diag-net").textContent =
      `Captura: ${Math.round(s.fps)} fps · ${s.width}×${s.height} · ${s.raw_mb_per_s.toFixed(1)} MB/s (crudo)`;
  });

  // Formatea el ID remoto mientras se escribe. Si lleva un punto, es una IP
  // local: se deja tal cual (no se agrupa en bloques de 3).
  $("peer-id").addEventListener("input", (e) => {
    const before = e.target.value;
    if (before.includes(".")) return;
    const pos = e.target.selectionStart;
    e.target.value = groupId(before);
    // Reajuste simple del cursor al final si crecio.
    if (pos >= before.length) e.target.selectionStart = e.target.selectionEnd = e.target.value.length;
  });

  // ---- Dispositivos guardados (libreta tipo AnyDesk) ----------------------
  // Se persisten en localStorage (sobrevive reinicios). Se guardan solos al
  // conectar y se pueden renombrar/eliminar/añadir a mano.
  // En linea si responde por LAN (descubrimiento UDP) O esta registrado en el
  // rendezvous ahora mismo (internet). Cualquiera de los dos alcanza.
  function checkDeviceOnline(id) {
    const checks = [invoke("check_online_lan", { peerId: id }).catch(() => false)];
    if (window.OtisRTC && OtisRTC.isConfigured()) checks.push(OtisRTC.checkPresence(id));
    return Promise.all(checks).then((results) => results.some(Boolean));
  }

  const Devices = (function () {
    const KEY = "otis_devices";
    const THUMB_KEY = "otis_thumbs";

    function loadThumbs() {
      try { return JSON.parse(localStorage.getItem(THUMB_KEY) || "{}"); } catch (_) { return {}; }
    }
    function saveThumbnail(id, dataUrl) {
      id = String(id).replace(/\D/g, "");
      if (!id) return;
      const map = loadThumbs();
      map[id] = dataUrl;
      try { localStorage.setItem(THUMB_KEY, JSON.stringify(map)); } catch (_) { /* cuota llena: ignorar */ }
    }
    function getThumbnail(id) { return loadThumbs()[String(id).replace(/\D/g, "")] || null; }

    function load() {
      try { return JSON.parse(localStorage.getItem(KEY) || "[]"); } catch (_) { return []; }
    }
    function save(arr) { localStorage.setItem(KEY, JSON.stringify(arr)); }

    function upsert(id, alias, profile) {
      id = String(id).replace(/\D/g, "");
      if (!id) return;
      const arr = load();
      const d = arr.find((x) => x.id === id);
      if (d) {
        d.ts = Date.now();
        if (alias) d.alias = alias;
        if (profile) d.profile = profile;
      } else {
        arr.push({ id, alias: alias || ("Equipo " + groupId(id)), profile: profile || "ultralight", ts: Date.now() });
      }
      save(arr);
    }
    function remove(id) {
      save(load().filter((d) => d.id !== id));
      const map = loadThumbs();
      delete map[id];
      try { localStorage.setItem(THUMB_KEY, JSON.stringify(map)); } catch (_) {}
    }
    function rename(id, alias) {
      const arr = load();
      const d = arr.find((x) => x.id === id);
      if (d) { d.alias = alias; save(arr); }
    }
    function recent() { return load().sort((a, b) => b.ts - a.ts); }

    // Accesos rapidos en la pantalla Conectar (los 4 mas recientes).
    function renderQuick() {
      const wrap = $("saved-quick");
      if (!wrap) return;
      wrap.innerHTML = "";
      const list = recent().slice(0, 4);
      if (list.length === 0) {
        wrap.innerHTML = '<span class="muted" style="font-size:11.5px">ninguno aún</span>';
        return;
      }
      list.forEach((d) => {
        const b = document.createElement("button");
        b.className = "btn btn-secondary saved-quick-btn";
        b.type = "button";
        b.textContent = d.alias;
        b.title = groupId(d.id);
        b.addEventListener("click", () => startConnect(d.id, d.profile));
        wrap.appendChild(b);
      });
    }

    // Libreta completa en la pestaña Dispositivos.
    function renderTab() {
      const listEl = $("devices-list");
      const emptyEl = $("devices-empty");
      if (!listEl) return;
      const q = ($("dev-search") ? $("dev-search").value : "").toLowerCase().replace(/\s/g, "");
      let list = recent();
      if (q) list = list.filter((d) => d.alias.toLowerCase().includes(q) || d.id.includes(q));

      listEl.innerHTML = "";
      if (emptyEl) emptyEl.classList.toggle("hidden", recent().length !== 0);

      list.forEach((d) => {
        const card = document.createElement("div");
        card.className = "blueprint dev-card";
        const thumb = getThumbnail(d.id);
        card.innerHTML =
          '<i class="corner tl"></i><i class="corner tr"></i><i class="corner bl"></i><i class="corner br"></i>' +
          (thumb
            ? `<div class="dev-thumb"><img src="${thumb}" alt="" /></div>`
            : `<div class="dev-thumb dev-thumb-empty">sin captura aún</div>`) +
          `<div class="dev-status"><span class="dot off"></span><span class="dev-status-text">comprobando…</span></div>` +
          `<div class="dev-name"></div>` +
          `<div class="dev-id num"></div>` +
          `<div class="dev-meta">Perfil: ${PROFILE_LABEL[d.profile] || "Ultraligero"}</div>` +
          `<div class="dev-actions"></div>`;
        card.querySelector(".dev-name").textContent = d.alias;
        card.querySelector(".dev-id").textContent = groupId(d.id);
        checkDeviceOnline(d.id).then((online) => {
          card.querySelector(".dev-status .dot").classList.toggle("off", !online);
          card.querySelector(".dev-status-text").textContent = online ? "en línea" : "desconectado";
        });

        const actions = card.querySelector(".dev-actions");
        const bConn = document.createElement("button");
        bConn.className = "btn btn-primary"; bConn.type = "button"; bConn.textContent = "Conectar";
        bConn.addEventListener("click", () => startConnect(d.id, d.profile));
        const bRen = document.createElement("button");
        bRen.className = "btn btn-secondary"; bRen.type = "button"; bRen.textContent = "Renombrar";
        bRen.addEventListener("click", () => {
          const name = window.prompt("Nombre del equipo:", d.alias);
          if (name && name.trim()) { rename(d.id, name.trim()); renderAll(); }
        });
        const bDel = document.createElement("button");
        bDel.className = "btn btn-ghost"; bDel.type = "button"; bDel.textContent = "Eliminar";
        bDel.addEventListener("click", () => { remove(d.id); renderAll(); });
        actions.append(bConn, bRen, bDel);
        listEl.appendChild(card);
      });
    }

    function renderAll() { renderQuick(); renderTab(); }

    return { upsert, remove, rename, recent, renderAll, renderQuick, renderTab, saveThumbnail, getThumbnail };
  })();

  // Buscador y alta manual en la pestaña Dispositivos.
  if ($("dev-search")) $("dev-search").addEventListener("input", () => Devices.renderTab());
  if ($("dev-add")) {
    $("dev-add").addEventListener("click", () => {
      const id = ($("dev-add-id").value || "").replace(/\D/g, "");
      if (id.length < 9) { toast("Introduce un ID de 9 dígitos"); return; }
      Devices.upsert(id, null, "ultralight");
      $("dev-add-id").value = "";
      Devices.renderAll();
      toast("Equipo guardado");
    });
  }

  // ---- Escaner de dispositivos en la red local -----------------------------
  const btnScanNet = $("btn-scan-net");
  if (btnScanNet) {
    btnScanNet.addEventListener("click", async () => {
      btnScanNet.disabled = true;
      const original = btnScanNet.textContent;
      btnScanNet.textContent = "Escaneando…";
      try {
        const devices = await invoke("scan_network");
        renderNetScan(devices || []);
        toast(`${(devices || []).length} dispositivo(s) encontrado(s)`);
      } catch (e) {
        toast("No disponible en previsualización");
      } finally {
        btnScanNet.disabled = false;
        btnScanNet.textContent = original;
      }
    });
  }

  function renderNetScan(devices) {
    const listEl = $("netscan-list");
    const emptyEl = $("netscan-empty");
    if (!listEl) return;
    listEl.innerHTML = "";
    emptyEl.classList.toggle("hidden", devices.length !== 0);
    devices.forEach((d) => {
      const card = document.createElement("div");
      card.className = "blueprint dev-card";
      card.innerHTML =
        '<i class="corner tl"></i><i class="corner tr"></i><i class="corner bl"></i><i class="corner br"></i>' +
        `<div class="dev-name"></div>` +
        `<div class="dev-id num"></div>` +
        `<div class="dev-meta"></div>`;
      card.querySelector(".dev-name").textContent = d.hostname || d.vendor || "Dispositivo";
      card.querySelector(".dev-id").textContent = d.ip;
      card.querySelector(".dev-meta").textContent = `MAC ${d.mac} · ${d.vendor}`;
      listEl.appendChild(card);
    });
  }

  // ---- Ajustes: servidor rendezvous (modo internet) ----------------------
  function updateNetModeLabel() {
    const configured = window.OtisRTC && OtisRTC.isConfigured();
    const el = $("net-mode");
    if (el) {
      el.textContent = configured ? "Modo internet (P2P) activo" : "Modo LAN (sin servidor)";
      el.className = "tag " + (configured ? "tag-accent" : "tag-outline");
    }
    const input = $("rv-url");
    if (input && !input.value && configured) input.value = OtisRTC.rendezvousUrl();
  }
  const rvSave = $("rv-save");
  if (rvSave) {
    rvSave.addEventListener("click", () => {
      const url = $("rv-url").value.trim();
      OtisRTC.setRendezvous(url);
      updateNetModeLabel();
      if (url && myDeviceId) OtisRTC.connectSignaling(myDeviceId);
      toast(url ? "Servidor guardado · modo internet" : "Servidor borrado · modo LAN");
    });
  }

  // ---- Ajustes: servidores TURN/ICE propios (avanzado) -------------------
  // Si el vídeo no llega por internet suele ser que el hole punching P2P no
  // funciona (NAT simétrico / CGNAT) y hace falta un TURN. Aquí se pega el
  // array `iceServers` en JSON; se AÑADE a los de fábrica. Vacío = solo fábrica.
  const iceInput = $("ice-url");
  if (iceInput) {
    try { iceInput.value = localStorage.getItem("otis_ice") || ""; } catch (_) {}
  }
  const iceSave = $("ice-save");
  if (iceSave) {
    iceSave.addEventListener("click", () => {
      const v = (iceInput.value || "").trim();
      if (!v) {
        try { localStorage.removeItem("otis_ice"); } catch (_) {}
        toast("Servidores ICE: usando los de fábrica");
        return;
      }
      try {
        const parsed = JSON.parse(v);
        if (!Array.isArray(parsed)) throw new Error("tiene que ser un array");
        localStorage.setItem("otis_ice", v);
        toast("Servidores ICE guardados (se aplican en la próxima conexión)");
      } catch (e) {
        toast("JSON de ICE inválido: " + e.message);
      }
    });
  }

  // ---- Controles de ventana ----------------------------------------------
  if (currentWindow) {
    $("win-min").addEventListener("click", () => currentWindow.minimize());
    $("win-max").addEventListener("click", () => currentWindow.toggleMaximize());
    $("win-close").addEventListener("click", () => currentWindow.close());
  }

  // ---- Arranque -----------------------------------------------------------
  bootstrap();
  checkForUpdate();
  refreshMetrics();
  Devices.renderAll();
  updateNetModeLabel();
  // Metricas cada 2s (barato en equipos lentos, solo refresca el propio proceso).
  setInterval(refreshMetrics, 2000);
})();
