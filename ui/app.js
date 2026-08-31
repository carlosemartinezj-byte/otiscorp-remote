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
  function startConnect(rawId, profile) {
    const peer = String(rawId).replace(/\D/g, "");
    if (peer.length < 9) {
      toast("Introduce un ID de 9 dígitos");
      return;
    }
    profile = profile || currentProfile();
    // Guarda/actualiza el equipo para la proxima (como AnyDesk).
    Devices.upsert(peer, null, profile);
    Devices.renderAll();
    $("conn-status").textContent = `Conectando a ${groupId(peer)}…`;
    // Por internet (WebRTC) si hay servidor configurado; si no, LAN directo.
    if (window.OtisRTC && OtisRTC.isConfigured()) {
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
    RemoteSession.open("Sesión · " + groupId(peer), profile, driver);
    unlisteners.push(await listen("remote-frame", (e) => {
      const p = e.payload || {};
      if (p.jpeg) RemoteSession.drawJpegB64(p.jpeg, p.width, p.height);
    }));
    unlisteners.push(await listen("remote-metrics", (e) => RemoteSession.setMetrics(e.payload || {})));
    try {
      await invoke("connect_peer", { peerId: peer, profile });
    } catch (e) {
      toast("No se pudo conectar: " + e);
      RemoteSession.close();
    }
  }

  // Conexion por internet (WebRTC P2P vía rendezvous).
  function connectViaRTC(peer, profile) {
    const driver = {
      sendInput: (ev) => OtisRTC.sendInput(ev),
      close: () => OtisRTC.disconnect(),
    };
    RemoteSession.open("Sesión · " + groupId(peer), profile, driver);
    OtisRTC.connect(peer, profile, {
      onFrame: (bmp) => RemoteSession.drawBitmap(bmp),
      onMetrics: (m) => RemoteSession.setMetrics(m),
      onOpen: () => { $("conn-status").textContent = "Conectado (P2P)"; },
      onClose: () => RemoteSession.close(),
      onError: (msg) => { toast(msg); RemoteSession.close(); },
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

    function fmtTime(ms) {
      const s = Math.floor(ms / 1000);
      return String(Math.floor(s / 60)).padStart(2, "0") + ":" + String(s % 60).padStart(2, "0");
    }

    function open(title, profile, drv) {
      driver = drv;
      active = true; controlOn = true;
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

    // Pinta un JPEG en base64 (camino LAN).
    function drawJpegB64(b64, w, h) {
      if (!active) return;
      if (w && h && (canvas.width !== w || canvas.height !== h)) {
        canvas.width = w; canvas.height = h;
      }
      img.onload = () => ctx.drawImage(img, 0, 0, canvas.width, canvas.height);
      img.src = "data:image/jpeg;base64," + b64;
    }

    function setMetrics(m) {
      $("session-metrics").textContent =
        `${Math.round(m.fps || 0)} fps · ${Math.round(m.latency_ms || 0)} ms · ${Math.round(m.kbps || 0)} kb/s`;
    }

    // Coordenadas normalizadas 0..1 respecto al contenido del canvas.
    function norm(e) {
      const r = canvas.getBoundingClientRect();
      const x = (e.clientX - r.left) / r.width;
      const y = (e.clientY - r.top) / r.height;
      return { x: Math.min(1, Math.max(0, x)), y: Math.min(1, Math.max(0, y)) };
    }
    function send(evt) {
      if (!controlOn || !active || !driver) return;
      try { driver.sendInput(evt); } catch (_) {}
    }

    const BTN = { 0: "left", 1: "middle", 2: "right" };
    canvas.addEventListener("mousemove", (e) => { const n = norm(e); send({ t: "move", x: n.x, y: n.y }); });
    canvas.addEventListener("mousedown", (e) => {
      e.preventDefault(); canvas.focus();
      const n = norm(e); send({ t: "move", x: n.x, y: n.y });
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

    function close() {
      if (!active && view.classList.contains("hidden")) return;
      active = false;
      clearInterval(timerId);
      if (driver) { try { driver.close(); } catch (_) {} driver = null; }
      view.classList.add("hidden");
      $("conn-status").textContent = "En línea · listo";
    }

    $("sb-end").addEventListener("click", close);
    $("sb-input").addEventListener("click", () => {
      controlOn = !controlOn;
      $("sb-input").textContent = "Control: " + (controlOn ? "on" : "off");
    });

    return { open, close, drawBitmap, drawJpegB64, setMetrics, isActive: () => active };
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

  // Formatea el ID remoto mientras se escribe.
  $("peer-id").addEventListener("input", (e) => {
    const pos = e.target.selectionStart;
    const before = e.target.value;
    e.target.value = groupId(before);
    // Reajuste simple del cursor al final si crecio.
    if (pos >= before.length) e.target.selectionStart = e.target.selectionEnd = e.target.value.length;
  });

  // ---- Dispositivos guardados (libreta tipo AnyDesk) ----------------------
  // Se persisten en localStorage (sobrevive reinicios). Se guardan solos al
  // conectar y se pueden renombrar/eliminar/añadir a mano.
  const Devices = (function () {
    const KEY = "otis_devices";

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
    function remove(id) { save(load().filter((d) => d.id !== id)); }
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
        card.innerHTML =
          '<i class="corner tl"></i><i class="corner tr"></i><i class="corner bl"></i><i class="corner br"></i>' +
          `<div class="dev-name"></div>` +
          `<div class="dev-id num"></div>` +
          `<div class="dev-meta">Perfil: ${PROFILE_LABEL[d.profile] || "Ultraligero"}</div>` +
          `<div class="dev-actions"></div>`;
        card.querySelector(".dev-name").textContent = d.alias;
        card.querySelector(".dev-id").textContent = groupId(d.id);

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

    return { upsert, remove, rename, recent, renderAll, renderQuick, renderTab };
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

  // ---- Controles de ventana ----------------------------------------------
  if (currentWindow) {
    $("win-min").addEventListener("click", () => currentWindow.minimize());
    $("win-max").addEventListener("click", () => currentWindow.toggleMaximize());
    $("win-close").addEventListener("click", () => currentWindow.close());
  }

  // ---- Arranque -----------------------------------------------------------
  bootstrap();
  refreshMetrics();
  Devices.renderAll();
  updateNetModeLabel();
  // Metricas cada 2s (barato en equipos lentos, solo refresca el propio proceso).
  setInterval(refreshMetrics, 2000);
})();
