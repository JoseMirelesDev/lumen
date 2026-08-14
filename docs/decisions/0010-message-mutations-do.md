# ADR 0010 — Message mutations propiedad del PresenceHubDO

Status: proposed · Date: 2026-08-13 · Scope: `plans/backend-v2` — resuelve conflicto Fase 2 ↔ Fase 3

## Context

Fase 2 define `PATCH/DELETE /api/messages/:id` contra la tabla `messages`
(1 fila = 1 mensaje). Fase 3 migra la persistencia a `message_blocks` (JSON
packed, ADR-004). Si el edit/delete sigue siendo REST contra `messages`, se
rompe: el mensaje ya no tiene fila propia. Además, dos edits concurrentes a
mensajes del mismo block causarían lost-update (read-modify-write no
atómico).

## Decision

**Todas las mutaciones de mensajes (send, edit, delete) pasan por el
PresenceHubDO** vía WS. El DO es single-threaded → serializa toda escritura
sobre un channel (buffer y blocks). El Worker REST queda **read-only** para
messages (GET, paginación, merge buffer).

- **Edit**: si el mensaje está en buffer → editar la entrada del buffer. Si
  está flusheado → leer block, modificar el JSON entry (content + edited_at),
  reescribir el block (1 read + 1 write, dentro del DO = atómico por
  serialización).
- **Delete**: idem — marcar `deleted: true` en el entry (placeholder
  "mensaje eliminado" en lectura).
- Broadcast de invalidación a suscritos (tag `c:<channelId>`) tras editar/
  borrar, para que los online vean el cambio sin recargar.
- Nuevos mensajes WS: `chat-edit` (`clientId, messageId, content`), `chat-delete`
  (`clientId, messageId`), con ACK igual que `chat` (ADR-005).

## Alternatives considered

- **REST + reescritura de block en el Worker**: dos Workers editan el mismo
  block concurrentemente → lost-update. Exigiría `db.batch()` transaccional
  con WHERE version — complejidad y sin serialización real entre isolates.
  Rechazado.
- **Tombstones en tabla separada** (`message_overrides`): evita reescribir
  el block pero complica lectura (merge en cada GET) y el FTS futuro.
  Rechazado: reescribir un block de 50 msgs (~5 KB JSON) es barato y simple.
- **No permitir editar mensajes flusheados**: UX rota. Rechazado.

## Consequences

- Positivo: sin carreras, un solo dueño de la escritura, Fase 2 y Fase 3
  compatibles (Fase 2 implementa REST como provisional; Fase 3 lo reemplaza
  por WS y elimina el REST de escritura), patrón consistente con ADR-005.
- Negativo: el DO hace más D1 writes (edit/delete = reescritura de block);
  con 500 users × 10 edits/día = ~5k writes/día extra (5% del límite —
  aceptable). El edit requiere author online (tiene WS) — correcto por
  definición.
- Riesgo: reescritura de block concurrente con un flush — el DO serializa
  ambos (single-threaded), el flush lee el buffer actual; un edit que
  llega después del flush reescribe el block. Sin race.
- Reversibilidad: alta — el protocolo WS es el contrato; volver a REST por
  mensaje = solo cambiar el transporte de la mutación.

## Revisit when

- CQRS real (reads desnormalizados) o migración Go (ADR-008): el hub Go
  reemplaza al DO con la misma semántica
