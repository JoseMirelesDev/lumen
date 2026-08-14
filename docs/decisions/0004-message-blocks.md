# ADR 0004 — Message blocks: persistencia de chat empacada

Status: proposed · Date: 2026-08-13 · Scope: `plans/backend-v2`

## Context

D1 free tier permite 100k writes/día. Un INSERT por mensaje limita el chat a
~1,000 usuarios activos. Se busca ~20x más capacidad sin salir del free tier.

## Decision

Los mensajes se acumulan en el `PresenceHubDO` (state.storage, durable) y se
flushean a D1 como **blocks** (`message_blocks`): 1 fila = JSON array de hasta
50 mensajes. Flush por umbral (50 msgs) o alarm (5 min). Lectura: 1 D1 read
por block, paginación cursor. Las mutaciones de mensajes (enviar/editar/
borrar) son propiedad del DO (ADR-010) — el DO serializa.

## Alternatives considered

- **1 fila por mensaje**: simple, pero 100k writes = techo ~1,000 users.
  Rechazado: no cumple el objetivo de escala.
- **Batch INSERT (multi-row)**: D1 cobra por fila, no por statement — no
  ahorra nada. Rechazado.
- **DO storage como almacén primario + D1 solo archivo**: cada lectura de
  histórico despierta el DO (1 request completo por GET); 50k msgs/día =
  50k requests = 50% del límite solo en lecturas. Rechazado: los blocks
  mantienen las lecturas en D1 (1 read por página).
- **R2 como almacén de mensajes**: writes Class A limitados (1M/mes) y
  latencia de escritura mayor; sin query. Rechazado.

## Consequences

- Positivo: 50x menos writes (100k → ~2k/día con 500 users); techo ~20,000
  usuarios (limitado por DO requests, ADR-003).
- Negativo: **editar/borrar un mensaje dentro de un block requiere reescribir
  el block** (1 read + 1 write, serializado por el DO); búsqueda FTS5 (Fase
  6.5) necesita filas por mensaje → se difiere a la era Go/Postgres
  (ADR-008); latency de persistencia de hasta 5 min (irrelevante para
  usuarios online que reciben por WS, ADR-005).
- Riesgo: pérdida de mensajes si el DO crashea entre `storage.put` y el flush
  → mitigado: storage durable, flush idempotente, ACK al cliente (ADR-005),
  retransmisión con dedup.
- Reversibilidad: **media** — el formato de block es interno a D1; el
  protocolo WS y la API REST no cambian. Migrar a Go = desempacar blocks.

## Revisit when

- Se necesite búsqueda full-text en D1 (implementar Fase 6.5 → evaluar tabla
  `messages_search` desnormalizada con FTS, aceptando 1 write/mensaje extra)
- Se migre a Go/PostgreSQL (ADR-008): blocks → filas normales
