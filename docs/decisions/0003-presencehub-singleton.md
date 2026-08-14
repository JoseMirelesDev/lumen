# ADR 0003 — PresenceHubDO: Durable Object singleton para presencia

Status: proposed · Date: 2026-08-13 · Scope: `plans/backend-v2`

## Context

El backend no tiene presencia global. `last_seen` se actualiza solo al login
(mentira de estado), la occupancy de voz vive dentro de cada `ChannelDO` y no
es consultable sin entrar al canal, y no hay canal de push hacia el cliente.
Se necesita: estado de amigos, miembros online por server, ver quién está en
voz sin entrar, y notificaciones real-time. Todo dentro del free tier
(100k DO requests/día; cada WS message = 1/20 request con Hibernation API).

## Decision

Nuevo DO `PresenceHubDO`, **una sola instancia** (`idFromName("hub")`), que
recibe un WebSocket por sesión de usuario (`/api/presence`). Estado en socket
attachments + tags (`userId`, `s:<serverId>`); cero `state.storage` para
presencia. Routing por `getWebSockets(tag)` → O(1), sin iterar sockets.

## Alternatives considered

- **Per-server DO** (1 instancia por server): N conexiones WS por usuario (1
  por server), friends cross-server requieren coordinación, más DO requests
  en connect. Rechazado: 3-10x más coste de conexión, misma memoria total.
- **Per-user DO**: 1 instancia por usuario, fan-out a amigos vía fetch
  cross-DO. Rechazado: más instancias, fetch por cambio de estado (cada fetch
  = 1 request completo), sin beneficio a esta escala.
- **Sin DO nuevo — polling REST + `last_seen` frecuente**: cero push real,
  ocupancy de voz requiere fetch a cada ChannelDO (cross-DO, N requests),
  amigos no reciben cambios en <1 min. Rechazado: no cumple las features.
- **Un solo DO global para presencia + chat**: es lo elegido (ver ADR-004/005).

## Consequences

- Positivo: 1 WS por usuario, broadcasts gratis, ~4,600 DO req/día con 500
  usuarios (4.6% del límite), cero storage para presencia, hiberna a coste
  ~0.
- Negativo: instancia única = punto único (un DO no escala horizontal); la
  instancia vive en un solo datacenter (latencia mayor para usuarios lejanos).
  El DO se bloquea en storage ops durante un flush (ADR-004) — aceptable:
  flush < 100 ms, una vez por canal cada 5 min.
- Riesgo: tags por socket limitados (Cloudflare no documenta tope duro; con
  ~50 servers + tags de suscripción por usuario es seguro; monitorear).
- Reversibilidad: **alta** — el protocolo WS es el contrato; migrar a Go
  (ADR-008) o a per-server DOs no cambia el cliente.

## Revisit when

- Usuarios concurrentes > 5,000 (DO requests acercándose a 50%+ del límite)
- O un solo DO se vuelve cuello de botella de CPU/latencia (>1k msg/s)
