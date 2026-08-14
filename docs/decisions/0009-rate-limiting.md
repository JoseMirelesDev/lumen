# ADR 0009 — Rate limiting: Cache API (REST) + socket attachments (WS)

Status: proposed · Date: 2026-08-13 · Scope: `plans/backend-v2` Fase 1

## Context

No hay rate limiting en ningún endpoint. Un atacante puede spamear register/
login (abuso de recursos), flood de mensajes, o abrir N WS. No hay Redis ni
KV con TTL garantizado en el free tier sin coste adicional (KV free: 100k
reads/día, 1k writes/día — insuficiente para un limiter por request).

## Decision

Dos mecanismos, ambos gratuitos:

- **REST** (Worker): sliding window en Cache API, key `rl:<ip>:<method>:<path>`.
  El Cache API es ilimitado y distribuido — no comparte estado entre
  isolates, pero la eviction agresiva solo reinicia la ventana (fail-open,
  aceptable). Coste: 1 cache read + 1 write por request.
- **WS** (DO): contadores en el socket attachment (per-socket, serializado
  por el DO). Coste: cero. Límites por tipo de mensaje (chat 10/10s, typing
  3/5s, voice-join 5/60s, dm-signal 20/60s).

## Alternatives considered

- **DO global rate-limiter**: 1 DO singleton al que cada request hace fetch
  → 1 request completo por request de API, duplicando el coste. Rechazado.
- **KV con TTL**: budget de writes insuficiente (1k/día). Rechazado.
- **Solo en el Worker**: no cubre el WS (el mensaje ni toca el Worker).
  Rechazado: ambos mecanismos son necesarios.

## Consequences

- Positivo: cero coste adicional, cobertura completa (REST + WS), límites
  por-IP y por-usuario, simple de auditar.
- Negativo: Cache API fail-open bajo eviction (un atacante puede reiniciar
  su ventana forzando eviction — mitigación: límites conservadores por
  defecto); no hay límites globales (solo por-IP/socket).
- Riesgo: `cf-connecting-ip` spoofeable detrás de proxies → usar el header
  CF (ya confiable en Workers).
- Reversibilidad: alta — mover a un limiter centralizado (Go, ADR-008) no
  cambia la interfaz.

## Revisit when

- Se detecte abuso distribuido (botnet) — requiere límites globales por
  cuenta, no por IP
- Migración a Go: Redis/process-local counters reemplazan Cache API
