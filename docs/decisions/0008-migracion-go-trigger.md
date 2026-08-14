# ADR 0008 — Migración a Go: trigger y límite del free tier

Status: proposed · Date: 2026-08-13 · Scope: `plans/backend-v2` Fase 6.10

## Context

El free tier de Cloudflare tiene techos duros: 100k Worker requests/día y
100k DO requests/día. Con la arquitectura ADR-003/004/005 el techo realista
es ~4,700 usuarios activos (Worker requests satura primero, según BUDGET.md
Escenario B). El usuario ya decidió que Go + PostgreSQL en VM es el target
final (discusión previa). Se necesita un trigger objetivo y un path de
migración que no obligue a reescribir el cliente.

## Decision

**Trigger**: migrar cuando Worker requests o DO requests superen el 70% del
límite diario durante 7 días consecutivos (medible en el dashboard CF).

**Path**: mantener Cloudflare como edge (Tunnel) → Go server (chi + coder/
websocket + pgx) implementa la MISMA API REST y el MISMO protocolo WS →
D1 portado a PostgreSQL (schema en `migrations/SPEC.md`) → R2 sigue en CF
(Go lo llama vía S3 API) → cliente solo cambia la URL base. El singleton
PresenceHubDO se convierte en hub in-memory Go (map[userId]*Conn) con la
misma semántica de tags/broadcast.

## Alternatives considered

- **Migrar ahora**: el usuario no tiene hosting VM aún; pagar por anticipado
  complejidad que el free tier cubre. Rechazado (reversibilidad: agregar
  Go hoy no se puede deshacer sin tirar trabajo).
- **Escalar CF a plan pago**: $5/mes+ y los límites suben pero siguen
  existiendo; el techo DO (10k req/s por instancia) no escala con un
  singleton de todos modos. Mantener como plan B si la migración se retrasa.
- **P2P-only para todo**: no es viable para canales grandes (ADR-006).

## Consequences

- Positivo: el plan define cuándo parar de invertir en CF; el cliente no
  cambia (contrato estable); Go da ~50k usuarios en una VM Oracle free.
- Negativo: mantener dos backends temporalmente (CF y Go) durante el
  solapamiento; port de blocks → filas (1 migración de datos).
- Riesgo: el trigger depende de métricas del dashboard (manual); automatizar
  con Analytics Engine query mensual (Fase 6).
- Reversibilidad: **alta** — mientras el contrato WS/REST sea idéntico, se
  puede volver a CF (los blocks siguen en D1 hasta el corte).

## Revisit when

- Usuarios activos > 4,000 (BUDGET.md Escenario B)
- Un DO singleton se convierta en cuello de botella medible
- Cloudflare cambie los límites del free tier
