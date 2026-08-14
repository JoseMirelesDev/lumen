# ADR 0005 — Chat real-time por WS presence con ACK + retransmisión

Status: proposed · Date: 2026-08-13 · Scope: `plans/backend-v2`

## Context

El chat hoy es REST-only (`POST /api/channels/:id/messages`): el emisor
escribe D1 y el receptor hace polling. No hay delivery instantáneo. Con
message blocks (ADR-004), el envío debe pasar por el `PresenceHubDO` de todos
modos (buffer). Se decide dónde vive el write path del chat.

## Decision

El envío de mensajes ocurre por el WS de presencia (`{ type: "chat" }`). El
DO: valida membership → buffer durable → broadcast a suscritos → ACK
(`chat-ack` con `clientId`) → flush (ADR-004). El cliente retransmite si no
recibe ACK en 3s (máx 3 intentos); dedup por `clientId` en el DO. El REST
`GET /api/channels/:id/messages` queda read-only (blocks + merge de buffer).

## Alternatives considered

- **Mantener REST POST**: el mensaje iría Worker→D1 y el broadcast exigiría
  otro salto (Worker→DO); 2 requests por mensaje (1 Worker + 1 DO) y
  latencia mayor. Rechazado: el WS ya está abierto (ADR-003), enviar por ahí
  cuesta 1/20 de request.
- **P2P delivery para todo el chat**: mesh no escala en canales con N
  receptores (N-1 conexiones por mensaje). Rechazado; P2P queda solo para DM
  y llamadas (ADR-006).
- **Sin ACK (fire-and-forget)**: pérdida silenciosa si el WS se corta tras
  `storage.put` y antes del ACK. Rechazado: UX de chat exige delivery
  confirmado.

## Consequences

- Positivo: 1/20 request por mensaje, delivery < 100 ms, sin polling,
  persistencia durable (ADR-004), escritura serializada por el DO (sin
  carreras de edit/delete).
- Negativo: el chat depende de la conexión WS (si el WS cae, el mensaje no
  sale — retransmisión lo cubre); el REST POST de Fase 2 queda deprecado
  para envío (se mantiene como fallback hasta Fase 3, luego se elimina).
- Riesgo: reintentos duplicados si el ACK se pierde → dedup por `clientId`
  (guardar últimos N clientIds en el attachment del socket).
- Reversibilidad: alta — el protocolo es el contrato; el cliente trata ACK
  como commit.

## Revisit when

- Se agregue CQRS real (reads desnormalizados en otro store)
- Migración a Go (ADR-008): mismo protocolo WS, backend distinto
