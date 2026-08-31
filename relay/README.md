# OtisCorp Relay

Servidor de encuentro/relay para conectar dos equipos **fuera de la red local**
(cada uno detrás de su router/NAT). Ambos clientes abren una conexión SALIENTE
al relay; este los empareja por el ID de 9 dígitos y hace de puente transparente.

- Sin estado compartido → correr **una sola máquina** (`fly scale count 1`).
- Fly.io termina TLS en el borde (puerto 443) → el tramo por internet va cifrado.

## Desplegar

```bash
fly deploy --remote-only
fly scale count 1        # el registro está en memoria; una sola instancia
```

Desplegado en: `otiscorp-relay.fly.dev:443` (región dfw / Dallas).
El cliente lo usa por defecto; se puede sobreescribir con la variable de entorno
`OTISCORP_RELAY=host:puerto`.
