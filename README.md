# OtisCorp Remote

Cliente de escritorio remoto ligero para Windows (tipo AnyDesk), construido con
**Tauri v2 + Rust (windows-rs)** y un frontend **vanilla sin bundler** para minimizar
RAM/CPU en reposo.

Arranca **directo a la pantalla principal**: no pide código de activación. Al primer
arranque genera automáticamente un **ID propio de 9 dígitos** (persistente) y deja el
**acceso desatendido activo** por defecto.

## Estado actual

Implementado:
- **Pantalla principal (Conectar)** fiel al sistema de diseño *Industry* (paneles
  blueprint con marcas de registro, tipografía Barlow Condensed / Barlow, acento acero).
- **Datos reales del equipo** vía backend Rust:
  - RAM total, marca de CPU y núcleos (`sysinfo`).
  - Versión de Windows leída del registro (`windows-registry`, familia windows-rs).
  - Detección de "gama baja" → perfil **Ultraligero** por defecto.
  - **Métricas en vivo del cliente** (RAM MB · CPU %) por polling cada 2 s.
- **Identidad auto-generada y persistente** (`identity.json` en el dir de datos de la app),
  contraseña de sesión de 4 dígitos regenerable, renombrado del puesto.
- Chrome de ventana propio (barra de título arrastrable, min/max/cerrar).
- **Motor de captura** (`capture.rs`): DXGI Desktop Duplication sobre D3D11 en un hilo
  dedicado — la GPU entrega el frame solo cuando cambia, así el consumo de CPU es mínimo.
- **Control de ratón/teclado** (`input.rs`): inyección con `SendInput` (mover, botones,
  rueda, teclas VK con flag de tecla extendida, texto Unicode); coordenadas normalizadas
  0..1 para funcionar entre resoluciones distintas.
- **Transporte de sesión** (`transport.rs`) — compartir pantalla + control, en **LAN** o
  fuera de ella vía el **relay** (ver más abajo). Mismo protocolo en los dos casos:
  - Descubrimiento por ID por **UDP broadcast** (puerto **49321**, solo LAN).
  - **Dos conexiones TCP por sesión**, no una: video (host → visor, puerto **49322** en
    LAN) y entrada/control (visor ↔ host, puerto **49323** en LAN). Antes iban
    multiplexadas en un solo socket y un frame grande podía retrasar el clic que llegaba
    justo detrás; separarlas fue el cambio que más se nota al usarlo.
  - Framing `[tipo][longitud][payload]` en ambas.
  - El **host** codifica a **H.264** (Media Foundation, hardware si el equipo tiene un
    encoder síncrono compatible — si no, cae al encoder por software que trae Windows) y
    reenvía a `SendInput` los eventos de entrada del visor. Si el equipo no tiene *ningún*
    encoder H.264 disponible (fuera de Windows, o un caso raro), cae a MJPEG.
  - El **visor** decodifica con `VideoDecoder` de **WebCodecs** (Annex B directo, sin
    conversión a AVCC) y pinta cada frame en un canvas a pantalla completa; si el host usó
    el fallback MJPEG, pinta el JPEG directo. Captura ratón/teclado sobre el canvas.
  - **Cola de un solo hueco + descarte**: el hilo de captura nunca escribe en el socket de
    video; dejar el frame codificado en un buzón que un hilo aparte vacía. Si ese hilo no
    llegó a tiempo, se salta la codificación de ese frame (no se acumula retraso).
  - **Bitrate adaptativo**: el hilo escritor mide cuántos frames se saltaron por
    saturación y ajusta el bitrate objetivo del encoder cada ~1.5 s dentro del rango del
    perfil.
  - **Dirty rects de DXGI**: si sólo se movió el cursor (sin cambios de escritorio), no se
    codifica ni se envía nada; si cambió una región chica, sólo esa región se recorta y
    repinta en el lienzo NV12 persistente del encoder, no el frame completo.
  - Perfiles: **Ultraligero** (½ resolución, ~24 fps, ~0.2–1.5 Mbps), **Equilibrado**
    (nativa, ~30 fps, ~0.5–4 Mbps), **Nítido** (nativa, ~30 fps, ~1–8 Mbps) — bitrates de
    arranque del rango adaptativo, no fijos.
- **Conexión por internet vía relay** (`relay.rs`) — un servidor propio en Fly.io hace de
  rendezvous TLS: host y visor abren cada uno una conexión saliente (atraviesa NAT/firewall
  sin configurar nada) y el relay los empareja por ID. Las dos conexiones de la sesión
  (video y entrada) se tunelan cada una por separado, emparejadas por el mismo ID con un
  dígito de sufijo distinto — el relay no necesitó cambios para soportar esto.

Pendiente:
- **Cifrado TLS 1.3 del transporte en LAN** (en la LAN el tráfico de sesión va en claro; el
  del relay sí va cifrado, Fly.io termina TLS en el borde).
- **Encoder H.264 por hardware en modelo asíncrono**: el módulo `h264enc.rs` sólo soporta
  el modelo síncrono clásico de Media Foundation. Varios encoders de hardware modernos
  (confirmado en esta máquina) sólo se exponen en modelo asíncrono, así que hoy se
  descartan y se usa el encoder por software — funciona bien, pero ese software tiene un
  "lookahead" de arranque fijo de ~16 frames (~500 ms de pantalla congelada sólo al
  conectar, no en régimen estable) que ninguna propiedad de `ICodecAPI` logra eliminar.
  Soportar el modelo asíncrono (eventos `IMFAsyncCallback`) desbloquearía aceleración por
  hardware real y quitaría ese arranque lento.
- El camino **WebRTC por internet** (`webrtc.js`, servidor de señalización separado en
  `rendezvous/`) sigue en MJPEG — es una ruta paralela a la de arriba y no entró en esta
  migración a H.264.
- Vistas restantes: Archivos, Historial.

## Cómo se usa (dos equipos en la misma red local)

1. Instala y abre OtisCorp Remote en **ambos** equipos.
2. En el equipo que quieres **controlar** (host): apunta su **ID de 9 dígitos** (panel
   "01 / Tu puesto"). El acceso desatendido ya está activo.
3. En el equipo desde el que **controlas** (visor): escribe ese ID en "02 / Conectar a",
   elige el perfil y pulsa **Conectar**. Se abre la vista de pantalla completa con el
   escritorio remoto; el ratón y el teclado ya controlan el otro equipo.
4. Para pruebas también puedes escribir la **IP** del host en vez del ID.

> **Firewall de Windows:** la primera vez que el host escucha, Windows puede pedir permitir
> el acceso de red de la app. Hay que **permitirlo** (en redes privadas) para que las
> conexiones entrantes lleguen. Puertos usados: **UDP 49321** (descubrimiento), **TCP
> 49322** (video) y **TCP 49323** (entrada/control).

## Conexión por internet (fuera de la LAN) — P2P

Para conectar entre equipos en **redes distintas** (dos oficinas, casa↔oficina):

1. Despliega **una vez** el servidor rendezvous (carpeta `rendezvous/`, gratis en Fly.io/Render;
   ver `rendezvous/README.md`). Obtienes una URL `wss://...`.
2. En **ambos** equipos: pestaña **Ajustes** → pega esa URL → Guardar. (Se recuerda; solo una vez.)
3. A partir de ahí funciona **igual que en LAN**: escribes el ID de 9 dígitos y **Conectar**.

Cómo funciona: la app usa el **WebRTC** del propio WebView. El servidor solo intercambia el
saludo inicial; el vídeo y el control van **directos y cifrados (DTLS/SRTP)** entre los dos
equipos, con hole punching y STUN gratis de Google. Si no hay servidor configurado, la app
usa automáticamente el modo **LAN directo**.

## Dispositivos guardados (como AnyDesk)

Cada equipo al que conectas se **guarda solo** para la próxima vez:
- Accesos rápidos en la pantalla **Conectar** (los más recientes).
- Libreta completa en **Dispositivos**: conectar con un clic, renombrar, eliminar, o añadir por ID.
- Se persisten localmente (sobreviven reinicios).

## Compilar para macOS

El código ya es multiplataforma (captura con Core Graphics, control con CGEvent). El
instalador de Mac **debe compilarse en un Mac** (Windows no puede cross-compilar apps de Mac).

En el Mac:

1. Instala **Xcode Command Line Tools**: `xcode-select --install`.
2. Instala **Rust** (toolchain por defecto): https://rustup.rs
3. Copia esta carpeta `otiscorp-remote/` al Mac y dentro:

   ```bash
   npm install
   npm run build
   ```

   Genera `src-tauri/target/release/bundle/dmg/OtisCorp Remote_2.4.0_*.dmg` y el `.app`.

4. **Permisos de macOS** (la primera vez que se usa como host):
   - **Ajustes del Sistema → Privacidad y seguridad → Grabación de pantalla** → activa OtisCorp Remote.
   - **Ajustes del Sistema → Privacidad y seguridad → Accesibilidad** → activa OtisCorp Remote
     (necesario para controlar ratón/teclado).
   - Como la app no está firmada con cuenta de Apple de pago, la primera vez ábrela con
     **clic derecho → Abrir** para saltar Gatekeeper.

> Nota: los iconos `.icns` de Mac los genera Tauri; si quieres tu propio icono ejecuta
> `npm run tauri icon ruta/al/icono.png` antes de `npm run build`.

## Requisitos para compilar y ejecutar

El toolchain ya está instalado en esta máquina (rustup + cargo con toolchain MSVC,
VS 2022 Build Tools con C++, y Windows SDK). En un equipo nuevo haría falta:

1. Rust (incluye `cargo`): https://rustup.rs — toolchain MSVC.
2. Visual Studio Build Tools con "Desktop development with C++" + Windows SDK.
3. WebView2 (ya viene en Win10/11; el instalador lo descarga si falta).
4. En la carpeta `otiscorp-remote/`: `npm install`.

### Desarrollo (hot-reload de la UI)

```bash
npm run dev
```

### Compilar el instalador (.exe / NSIS)

```bash
npm run build
```

El binario y el instalador quedan en `src-tauri/target/release/`.

## Estructura

```
otiscorp-remote/
├─ package.json          # scripts (tauri dev/build) + @tauri-apps/cli
├─ ui/                   # frontend vanilla (sin framework, sin bundler)
│  ├─ index.html
│  ├─ app.js             # bootstrap, métricas, pestañas, acciones
│  └─ styles/app.css     # tokens + componentes del sistema Industry
└─ src-tauri/
   ├─ Cargo.toml         # release optimizado: opt-level z, lto, strip, panic abort
   ├─ tauri.conf.json    # ventana sin decoración, withGlobalTauri
   ├─ capabilities/      # permisos mínimos
   ├─ gen-icon.js        # genera icons/icon.ico (placeholder)
   └─ src/
      ├─ main.rs         # comandos Tauri (bootstrap, captura, input, sesión remota)
      ├─ identity.rs     # ID persistente auto-generado, contraseña de sesión
      ├─ sysprofile.rs   # perfilado real (sysinfo) + versión Windows (windows-rs)
      ├─ capture.rs      # motor de captura DXGI Desktop Duplication (hilo dedicado, dirty rects)
      ├─ h264enc.rs      # encoder H.264 (Media Foundation) + conversion BGRA -> NV12
      ├─ input.rs        # inyección de ratón/teclado con SendInput
      ├─ relay.rs        # cliente TLS del relay (rendezvous fuera de la LAN)
      └─ transport.rs    # descubrimiento LAN + sesión TCP (dos sockets: video H.264 + entrada)
```

## Notas de diseño para bajo consumo

- Frontend sin framework ni bundler → menos JS que parsear/ejecutar.
- `sysinfo` refresca **solo el proceso propio** en cada tick (no re-escanea todo el sistema).
- Perfil de release: `opt-level = "z"`, `lto`, `codegen-units = 1`, `strip`, `panic = "abort"`.
- Ventana WebView2 (ya presente en el SO) en lugar de empaquetar Chromium.

## Previsualizar la UI sin Rust

Para ver solo la interfaz (con datos de ejemplo) sin compilar, sirve `ui/` con
`node preview.js` y abre `http://localhost:4820`. El `app.js` detecta la ausencia de
backend Tauri y usa datos de previsualización.
