# OtisCorp Remote — Servidor rendezvous (señalización P2P)

Servidor diminuto que permite conectar **fuera de la red local**. Hace de "guía
telefónica": cada equipo se registra con su **ID de 9 dígitos**, y cuando uno quiere
conectar con otro, el servidor reenvía el saludo inicial para que ambos hagan
**hole punching** y hablen **directo P2P**. El vídeo y el control **no pasan por aquí**
(van directos entre las dos PCs), por eso consume casi nada y cabe en un plan gratuito.

## Qué necesitas

Desplegarlo **una vez** en cualquier sitio que dé una URL pública con HTTPS/WSS. La app
solo necesita esa URL (algo como `wss://tu-servidor.fly.dev`).

## Opción A — Fly.io (recomendada, tiene asignación gratuita)

1. Instala `flyctl`: https://fly.io/docs/hands-on/install-flyctl/
2. `fly auth signup` (crea cuenta) o `fly auth login`.
3. Edita `fly.toml` y cambia `app = "otiscorp-rendezvous"` por un nombre único tuyo.
4. Desde esta carpeta (`rendezvous/`):

   ```bash
   fly launch --copy-config --now
   ```

5. Tu URL será `https://<tu-app>.fly.dev`. En la app OtisCorp usa **`wss://<tu-app>.fly.dev`**.

## Opción B — Render (plan free)

1. Sube esta carpeta a un repo de GitHub.
2. En https://render.com → New → **Web Service** → conecta el repo.
3. Runtime: Node. Build: `npm install`. Start: `npm start`.
4. Tu URL será `https://<tu-app>.onrender.com` → en la app usa `wss://<tu-app>.onrender.com`.

   > Nota: el plan free de Render "duerme" tras inactividad; la app reintenta la
   > conexión, así que solo se nota un pequeño retraso la primera vez.

## Opción C — Cualquier VM (Oracle Always Free, etc.)

```bash
npm install
PORT=8080 npm start
```

Ponlo detrás de HTTPS (Caddy/Nginx) para tener `wss://`.

## Probar en local

```bash
npm install
npm start
# en otra terminal:
curl http://localhost:8080/health
```

## Protocolo (por si lo quieres tocar)

JSON sobre WebSocket:

| Sentido | Mensaje |
| --- | --- |
| cliente → servidor | `{ "type":"register", "id":"731204998" }` |
| servidor → cliente | `{ "type":"registered", "id":"731204998" }` |
| cliente → servidor | `{ "type":"signal", "to":"402118553", "data":{…} }` |
| servidor → destino | `{ "type":"signal", "from":"731204998", "data":{…} }` |
| servidor → cliente | `{ "type":"peer-offline", "to":"402118553" }` |

`data` es opaco para el servidor: los clientes meten ahí su endpoint público (STUN),
el nonce de hole punching y la petición de conexión.
