# BUDGET.md — Contabilidad del free tier

## Limites Cloudflare free tier (2026)

| Recurso | Limite | Notas |
|---|---|---|
| Worker requests | 100,000/dia | Se cuentan por request HTTP |
| DO requests | 100,000/dia | WS messages = 1/20 cada uno (Hibernation) |
| DO duration | 400,000 GB-s/mes | Hibernado = ~0 |
| DO storage | 1 GB | Solo buffers de chat |
| D1 reads | 5,000,000/dia | |
| D1 writes | 100,000/dia | 1 por fila modificada (row write) |
| D1 storage | 5 GB | |
| R2 storage | 10 GB | |
| R2 Class A ops | 1,000,000/mes | PUT/GET list |
| R2 Class B ops | 10,000,000/mes | GET individuales |
| Analytics Engine | 400 MB/dia | |
| Cache API | Ilimitado | |

## Escenarios de carga

### Escenario A: 500 usuarios activos/dia (objetivo inicial)

Asunciones:
- 500 usuarios hacen login y usan la app
- 100 mensajes/chat por usuario/dia = 50,000 mensajes
- 100 llamadas de voz de 20 min
- 200 DM conversations activas
- 50 servers

### DO requests

| Evento | Req/dia | Tipo |
|---|---|---|
| Conexiones presence (500 × 2) | 1,000 | Full |
| Conexiones voice (100 × 2) | 200 | Full |
| Status changes (500 × 2) | 50 | 1/20 |
| Voice join/leave (200) | 10 | 1/20 |
| Chat messages (50,000) | 2,500 | 1/20 |
| Typing (5,000) | 250 | 1/20 |
| DM signaling (200 × 10) | 100 | 1/20 |
| Voice signaling (100 × 80) | 400 | 1/20 |
| Subscribes/unsubscribes (1,000) | 50 | 1/20 |
| **Total** | **~4,560** | |

**4.6% del limite. Headroom: 21x.**

### D1

| Operacion | Writes/dia | Reads/dia |
|---|---|---|
| Chat (50,000 msgs / 50 por block) | 1,000 | 0 |
| Membership re-validation en chat (R4: 1 read/msg) | 0 | 50,000 |
| Message pagination (500 × 4 paginas) | 0 | 2,000 |
| Auth (500 registros/logins) | 500 | 500 |
| last_seen (500 disconnects) | 500 | 0 |
| Servers/channels CRUD | 100 | 0 |
| Server detail (cache hit 99%) | 0 | 50 |
| Friend ops | 100 | 200 |
| Refresh tokens | 250 | 250 |
| **Total** | **~2,450** | **~53,000** |

**Writes: 2.5% del limite. Reads: 1.06% del limite (5M). Headroom: 40x
writes, 94x reads.** La re-validación de membership (1 read por mensaje)
sigue dejando 94% del budget de reads libre.

### Worker requests

| Operacion | Req/dia |
|---|---|
| REST (auth, CRUD, messages load) | ~8,000 |
| WS upgrades (presence 500 + voice 100) | 600 |
| R2 asset serves (avatars) | ~2,000 |
| **Total** | **~10,600** |

**10.6% del limite. Headroom: 9x.**

### R2

| Operacion | Ops/mes |
|---|---|
| Avatar uploads (500 × 1) | 500 Class A |
| Avatar serves (2,000 × 30) | 60,000 Class B |
| Attachments | <5,000 Class A |
| **Total** | **~5,500 Class A / 60,000 Class B** |

**0.55% Class A, 0.6% Class B. Headroom: masivo.**

### Resumen Escenario A

| Recurso | Uso | Limite | % | OK |
|---|---|---|---|---|
| DO requests | 4,560 | 100k | 4.6% | ✅ |
| D1 writes | 2,450 | 100k | 2.5% | ✅ |
| D1 reads | 3,000 | 5M | 0.06% | ✅ |
| Worker requests | 10,600 | 100k | 10.6% | ✅ |
| R2 ops | 65,500 | 11M | 0.6% | ✅ |

---

### Escenario B: 5,000 usuarios activos/dia (techo realista)

Escalado lineal del A:

| Recurso | Uso | Limite | % | OK |
|---|---|---|---|---|
| DO requests | 45,600 | 100k | 45.6% | ✅ |
| D1 writes | 24,500 | 100k | 24.5% | ✅ |
| D1 reads | 30,000 | 5M | 0.6% | ✅ |
| Worker requests | 106,000 | 100k | **106%** | ❌ SATURADO |
| R2 ops | 655,000 | 11M | 6% | ✅ |

**Worker requests es el primer limite en saltar** (~4,700 users).
Mitigaciones si se acerca:
1. Cachear mas agresivo (TTL 5 min para server data)
2. Mover asset serves a un segundo Worker (los 100k son per-worker)
3. Batch de REST (el cliente pide menos, mas grande)

### Escenario C: 10,000 usuarios (llevar a Go)

| Recurso | Uso | Limite | % | OK |
|---|---|---|---|---|
| DO requests | 91,200 | 100k | 91% | ❌ |
| D1 writes | 49,000 | 100k | 49% | ✅ |
| Worker requests | 212,000 | 100k | **212%** | ❌ |

**A 10k usuarios, migrar a Go es obligatorio.** El plan asume que esto
sucede antes: los limites DO y Worker se saturan primero.

---

## Reglas de presupuesto (obligatorias)

1. **Nunca** escribir D1 en hot paths (mensaje, typing, presencia).
   Todo pasa por DO memory/attachments. D1 solo en flush y CRUD.

2. **Nunca** hacer D1 reads en loops. Batch siempre.

3. **Cache API** para todo dato que cambie menos de 1/min.

4. **WS es gratis al enviar.** Disenar client->server minimo.

5. **P2P primero** para traffic entre pares online.

6. **Monitor mensual** de las metricas del dashboard. Documentar aqui
   cada vez que se mida.

## Metricas a medir (dashboard Cloudflare)

| Metrica | Donde | Frecuencia |
|---|---|---|
| Worker requests | Workers > lumen-backend | Diario |
| DO requests | Durable Objects | Diario |
| D1 reads/writes | D1 > lumen-d1 | Diario |
| R2 ops | R2 > lumen-assets | Mensual |
| Analytics | Analytics > lumen_requests | Mensual |

---

## Mediciones reales por fase (implementación 2026-08-13/14)

### Fase 1 (seguridad) — sin cambios de consumo
Rate limit usa Cache API (gratis, ilimitado). Refresh tokens añaden 1 write por
login + 1 por rotación (~250-500 writes/día a 500 users — dentro del 2.5%
estimado). Verificado: smoke 30+ steps ALL PASS; 24 unit tests.

### Fase 2 (CRUD) — sin impacto
Mismo patrón de writes que el CRUD existente (1 fila por mutación). Soft delete
de DM requirió migración nueva `0004_dm_members_soft.sql` (marcador `deleted_at`
en dm_members): borrar la fila rompía la vista del otro lado (JOIN del
participante). Renumerado: oauth→0005, moderación→0006, polish→0007 (SPEC.md).

### Fase 3 (real-time) — medido contra D1 local
- **message_blocks**: 12/12 blocks recientes con count=50 exacto (flush por
  umbral post-push). Umbral + alarm de 5 min.
- **D1 writes**: 1 por block (50 msgs) + 1 last_seen por disconnect + 1 por
  reescritura de block editada/borrada (ADR-0010).
- **D1 reads**: 1 por mensaje (re-validación membership R4) + 1 por página de
  paginación + 1 por upgrade presence (batch de 2).
- **DO requests**: 1 full por conexión presence; 1/20 por mensaje WS.
- Coste real del flush intercalado: **0** (transacciones de storage por canal —
  ver reporte: bug encontrado y corregido).
- Punto de control: 101 mensajes → 2 blocks de 50 + 1 en buffer; paginación
  con cursor compuesto (last_at, id) devuelve el block anterior en 1 read. ✓
- **workerd/miniflare**: wrangler actualizado 4.118→4.123 (4.118 usaba
  miniflare 5.20260730.0-ALPHA que crasheaba el dev server tras varios runs).

### Fases 4-6 — sin impacto material
- **OAuth**: 1 write por state + 1 por callback + 1 por usuario OAuth creado
  (~0.5% del presupuesto). Rate limit 10/5min por IP en los endpoints.
- **Assets**: R2 — 500 Class A (uploads) y ~60k Class B (serves)/mes con 500
  users (0.6% del límite). Worker requests +2k/día máx (10% → 12.6%).
- **Moderación**: 1 write por ban/block/report (despreciable). Reports rate
  limit 5/día.
- **Fase 6**: reactions ~1 write por toggle + 1 read agregado; attachments
  1 Class A por upload; replies 0 extra (campo en el block JSON).

### Consumo estimado a 500 users (post-Fase 3, igual al presupuesto original)
| Recurso | Estimado | Límite | % |
|---|---|---|---|
| DO requests | ~4,560/día | 100k | 4.6% |
| D1 writes | ~2,450/día | 100k | 2.5% |
| D1 reads | ~53,000/día | 5M | 1.1% |
| Worker requests | ~10,600/día | 100k | 10.6% |
