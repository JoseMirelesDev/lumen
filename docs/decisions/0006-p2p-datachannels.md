# ADR 0006 — P2P DataChannels para DM y tráfico in-call

Status: proposed · Date: 2026-08-13 · Scope: `plans/backend-v2`

## Context

El servidor no debería mediar tráfico entre pares que ya están conectados.
Typing y mensajes de DM/l llamada son efímeros y de alta frecuencia; cada uno
mediado por el servidor cuesta 1/20 de DO request y añade latencia. El
cliente ya tiene WebRTC (webrtc-rs) con la maquinaria de peer connection.

## Decision

- **In-call**: cada `RTCPeerConnection` de voz crea `createDataChannel("chat")`
  — typing y mensajes de la llamada fluyen P2P. Coste servidor: cero.
- **DM**: al abrir un DM con alguien online, se establece una peer connection
  data-only (sin tracks de audio) usando el relay `dm-signal` del
  PresenceHubDO (ADR-003). Typing y delivery van P2P; la persistencia sigue
  siendo server-side (el emisor hace el flush vía WS o REST según ADR-005).
- El servidor NUNCA ve el contenido del DataChannel; solo el signaling
  inicial.

## Alternatives considered

- **Relay server de DM typing**: simple pero cobra 1/20 por mensaje y añade
  latencia; el DM es 2 pares → mesh trivial. Rechazado.
- **Broadcast del servidor para DM**: mismo problema + el servidor no necesita
  saber quién está en un DM abierto. Rechazado.
- **Nada (polling para typing)**: UX pobre. Rechazado.

## Consequences

- Positivo: ~0 DO requests para typing/DM real-time; latencia mínima; el
  servidor escala con usuarios online, no con mensajes efímeros.
- Negativo: complejidad WebRTC en el cliente (data-only peer connection,
  ICE/NAT como en voz); si el NAT es simétrico, requiere TURN (ya disponible
  vía Cloudflare Realtime).
- Riesgo: 2 peers = 1 conexión P2P más por DM abierto (RAM/CPU del cliente);
  con 10 DMs abiertos = 10 conexiones — aceptable; cerrar al cambiar de vista.
- Reversibilidad: **alta** — el signaling es el mismo relay; volver a
  servidor-mediado es solo que el cliente mande por WS en vez del channel.

## Revisit when

- Se agregue chat de grupo con < 6 participantes (mesh sigue siendo viable)
- El cliente muestre > 10 DMs abiertos simultáneamente en máquinas débiles
