# Architecture Review — Plan backend-v2 vs skills directives

Status: **REVISADO** · Date: 2026-08-13
Skills aplicadas: `software-architecture` (SOLID, ADRs, anti-patrones, health
audit) y `software-architect` (sin astronautics, trade-offs nombrados, dominio
primero, reversibilidad, decisiones documentadas).

## 1. Cumplimiento por directiva

| Directiva | Estado | Evidencia |
|---|---|---|
| Decisiones documentadas (ADRs con WHY) | ✅ **CORREGIDO** | ADR-003..0010 creados en `docs/decisions/` |
| Trade-offs nombrados (qué se pierde) | ✅ | Cada ADR tiene Consequences negativos + Revisit when |
| Reversibilidad | ✅ | Cada ADR declara nivel de reversibilidad |
| Sin architecture astronautics | ✅ | 2 DOs, 0 interfaces sobre-abstraídas, 0 microservicios |
| Dominio primero | ⚠️ Implícito | Ver §2 — falta mapeo formal de bounded contexts |
| SRP | ⚠️ **CORREGIDO** | PresenceHubDO 4 responsabilidades → módulos puros (ver P3) |
| Testabilidad | ⚠️ **CORREGIDO** | Extracción de lógica pura + tests unitarios (ver P3/P7) |
| Anti-patrones chequeados | ✅ | Ver §4 |
| Riesgos de refactor estructural | ⚠️ Parcial | Gates de compilación por fase ya existen (smoke/cargo test); se añade política de migraciones (P6) |

## 2. Bounded contexts (dominio primero)

```
Identity/Auth     users, refresh_tokens, oauth_states        D1 (source of truth)
Servers/Channels  servers, channels, categories, roles       D1
Messaging         message_blocks, buffer del DO              DO escribe, D1 persiste
Presence          attachments + tags del PresenceHubDO       DO memory (efímero por diseño)
Voice             ChannelDO + WebRTC P2P + TURN              DO coordina, media P2P
Social            friendships, dm_members, blocks            D1
Moderation        server_bans, reports                       D1
Assets            avatars, icons, attachments                R2
```

Cada dominio tiene dueño de datos claro. Presencia es deliberadamente
efímera (no sobrevive al DO — correcto: no hay nadie online si el hub
reinicia; los clientes reconectan).

## 3. Architecture Health Score (arquitectura target)

```
Architecture Health Score
=========================

Coupling              ██████░░░░  [Good]      - DOs comunican solo por WS/fetch; protocol package shared; UI→Core→Voice unidireccional
Cohesion              █████░░░░░  [Good/Needs Work] - PresenceHubDO 4 concerns (P3, mitigado con módulos puros)
Abstraction Level     ███████░░░  [Good]      - 2 DOs, sin repositorios ficticios, sin interfaces sobre-abstraídas
Testability           ███░░░░░░░  [Needs Work] - lógica DO embebida (P3/P7); mitigado: extraer puros + miniflare integration
Pattern Consistency   ██████░░░░  [Good]      - Hibernation + tags uniforme en ambos DOs; validación central en validation.ts
```

Veredicto: la arquitectura target es coherente y ajustada al free tier; los
dos problemas reales eran la falta de ADRs (corregido) y la testabilidad de
la lógica del DO (corregido con extracción de módulos puros).

## 4. Anti-pattern checklist

| Anti-patrón | ¿Aplica? | Nota |
|---|---|---|
| Repository pattern sobre tabla simple | ❌ No | `db.ts` es un módulo de queries, no un repo ficticio |
| Event-driven en todo | ❌ No | WS broadcast solo donde hay real-time; CRUD sigue REST síncrono |
| Microservicios a 3 devs | ❌ No | Serverless DOs; 2 clases con frontera clara |
| Interface en cada clase | ❌ No | Cero interfaces; se abstrae solo el rate limiting (módulo) |
| God object | ⚠️ Parcial | PresenceHubDO (P3) — mitigado |
| Async everywhere | ❌ No | Happy path de CRUD síncrono; async solo WS/persistencia real-time |
| Sin dueño de datos | ❌ No | §2 mapea ownership por dominio |

## 5. Findings

| ID | Sev | Finding | Impact | Fix | Effort | Unlocks |
|----|-----|---------|--------|-----|--------|---------|
| P1 | High | Decisiones sin ADR | Sin WHY/alternativas; no revisables | ADR-003..0010 creados | Moderate | Gobernanza |
| P2 | High | Fase 2 edit/delete REST ↔ Fase 3 blocks | Rutas rotas tras migración; lost-update | ADR-0010: mutaciones por el DO (WS), REST read-only | Moderate | Messaging coherente |
| P3 | Med | PresenceHubDO = presencia + buffer + DM relay + rate limit (SRP) | Cambia por 4 razones; lógica no testeable | Extraer `do/lib/buffer.ts`, `do/lib/ws-rate-limit.ts`, `do/lib/presence-utils.ts` (puros) | Moderate | Unit tests sin DO runtime |
| P4 | Med | FTS5 (Fase 6.5) vs message_blocks | FTS requiere fila por mensaje; blocks lo impiden | Diferir full-text a Go/Postgres; LIKE sobre blocks como MVP | Low | Roadmap honesto |
| P5 | Low | Cursor paginación solo por `last_at` | Colisiones de timestamp saltan/duplican | Cursor compuesto `(last_at, id)` | Quick | Paginación correcta |
| P6 | Low | Sin política de migraciones | ALTER forward-only sin plan de undo | Política forward-only + gates por fase (SPEC.md) | Quick | Ops segura |
| P7 | Low | Testabilidad DO sin diseño | Tests unitarios imposibles | Igual que P3 + integration tests con miniflare | Moderate | Suite de tests |

## 6. Cambios aplicados a los documentos del plan

| Documento | Cambio |
|---|---|
| `docs/decisions/0003..0010` | **NUEVOS** — 8 ADRs (singleton, blocks, chat WS+ACK, P2P, refresh, Go trigger, rate limiting, message mutations) |
| `plans/backend-v2/ARCHITECTURE.md` | §3.3 cursor compuesto `(last_at, id)`; §3.4 mutaciones de mensajes propiedad del DO |
| `plans/backend-v2/phases/02-crud.md` | Edit/delete REST marcado PROVISIONAL (reemplazado por WS en Fase 3, ADR-0010) |
| `plans/backend-v2/phases/03-realtime.md` | Mensajes WS `chat-edit`/`chat-delete`; módulos puros; cursor compuesto |
| `plans/backend-v2/protocol/presence-v2.md` | Tipos `chat-edit`/`chat-delete` |
| `plans/backend-v2/migrations/SPEC.md` | Política forward-only + gates de verificación |
| `plans/backend-v2/CHECKLIST.md` | Items para ADRs, módulos puros, mutaciones WS, cursor compuesto |
| `plans/backend-v2/README.md` | Referencia a ADRs + health score |

## 7. Decisiones que el plan tomó bien (verificadas contra skills)

- **2 DOs, no más**: la frontera PresenceHub (estado global) ↔ ChannelDO
  (coordinación de voz) es la mínima que satisface las features. Un solo DO
  hub para todo (voz incluida) habría sido un cuello de botella; uno por
  feature habría sido astronautics.
- **P2P solo donde el mesh es trivial** (2 pares): no se intentó P2P para
  canales — la regla "use a pattern if it solves an actual pain point" se
  respeta.
- **Flush por umbral + alarm, no cron**: sin timers en DO = hibernación
  intacta = coste ~0 idle. Alineado con la restricción del runtime.
- **Migración a Go como ADR con trigger medible** (ADR-008), no como
  promesa: se nombra cuándo parar de invertir en CF.

## 8. Riesgo residual (documentado, aceptado)

| Riesgo | Aceptado porque | Mitigación |
|---|---|---|
| Singleton DO = punto único | Escala free tier < 5k users | ADR-008 trigger; protocolo migrable |
| Blocks complican FTS | Search es Fase 6, no core | Diferir a Go/Postgres (tsvector) |
| Rate limit fail-open en Cache API | Eviction solo reinicia ventana | Límites conservadores por defecto |
| Kick/ban no instantáneo en WS | **CORREGIDO (R4)**: re-validación de membership en cada chat/voice-join (1 D1 read/msg = 1% budget) | Cierre de socket 4403 "kicked" |
| Edit/delete reescribe blocks | Writes extra ~5% del budget | Aceptable; serializado por el DO + rate limit |
