# Fase 5 — Moderacion

Prioridad: **MEDIA** | Dependencias: Fase 2 | Duracion estimada: 2-3 dias

## Objetivo

Herramientas de moderacion: bans, blocks, reports, transferencia de
ownership, y manejo de servers huerfanos.

## Archivos

| Archivo | Cambio |
|---|---|
| `src/index.ts` | Rutas de moderacion |
| `src/db.ts` | Queries bans/blocks/reports |
| `migrations/0005_moderation.sql` | Tablas nuevas |
| `src/validation.ts` | Validators de reason |
| `packages/protocol/src/index.ts` | Tipos |
| `apps/lumen-slint/ui/` | UI de moderacion |
| `crates/lumen-core/src/api.rs` | API calls |

## Tareas

### 5.1 Schema: migration 0005_moderation.sql

```sql
CREATE TABLE IF NOT EXISTS server_bans (
  server_id TEXT NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  reason TEXT,
  banned_by TEXT NOT NULL REFERENCES users(id),
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  PRIMARY KEY (server_id, user_id)
);
CREATE INDEX IF NOT EXISTS idx_bans_server ON server_bans(server_id);
CREATE INDEX IF NOT EXISTS idx_bans_user ON server_bans(user_id);

CREATE TABLE IF NOT EXISTS blocks (
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  blocked_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  PRIMARY KEY (user_id, blocked_id)
);
CREATE INDEX IF NOT EXISTS idx_blocks_blocked ON blocks(blocked_id);

CREATE TABLE IF NOT EXISTS reports (
  id TEXT PRIMARY KEY,
  reporter_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  target_type TEXT NOT NULL CHECK (target_type IN ('message', 'user', 'server')),
  target_id TEXT NOT NULL,
  reason TEXT,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_reports_target ON reports(target_type, target_id);
```

### 5.2 Rutas

```
// bans
POST   /api/servers/:id/bans          { userId, reason? }  -> 201   (owner)
DELETE /api/servers/:id/bans/:userId  -> 204                       (owner)
GET    /api/servers/:id/bans          -> [ban]                     (owner)

// blocks
POST   /api/blocks                    { userId }   -> 201
DELETE /api/blocks/:userId            -> 204

// reports
POST   /api/reports                   { targetType, targetId, reason? } -> 201

// ownership
POST   /api/servers/:id/transfer      { userId }   -> 200   (owner -> miembro)
```

### 5.3 Reglas de negocio

| Operacion | Reglas |
|---|---|
| Ban | Owner solo. Al banear: kick + impedir join por invite. `canAccessChannel` y `join` chequean `server_bans`. Los WS del baneado en ese server: desconectar (el ChannelDO/PresenceHubDO no puede saber sin consultar — solucion: el Worker consulta bans en el upgrade de WS y en cada join; el kick inmediato es best-effort, el bloqueo real es en acceso) |
| Unban | Owner. Re-join permitido |
| Block | Cualquier user. Efectos: no recibir friend requests, no recibir DMs, no ver mensajes del bloqueado en channels compartidos (filtrar en lectura — opcional v1: solo bloquear DMs/friends) |
| Report | Cualquier user. Sin auto-action; queda en tabla para review manual (Fase 6: admin panel) |
| Transfer | Owner -> cualquier miembro. El nuevo owner recibe ownership; el viejo pasa a miembro. No transferible si el target esta baneado |
| Server huerfano | Si el owner borra su cuenta (Fase 2), `servers.owner_id` queda apuntando a user soft-deleted. En este caso, cualquier miembro puede llamar `POST /api/servers/:id/transfer` sin ser owner (el check es `owner deleted OR user is owner`) |

### 5.4 Integracion con presencia

- Kick: el Worker hace DELETE member + fetch al PresenceHubDO con
  `?kick=<userId>&server=<serverId>` — el DO cierra el WS del usuario en
  ese server tag (no puede cerrar por tag; alternativa: el DO guarda una
  lista `kicked:<serverId>` en storage con TTL, y al proximo mensaje del
  socket baneado, lo cierra. Simpler: el cliente recibe `error kicked`
  via broadcast, y el acceso esta bloqueado en el proximo upgrade).
- Documentar: kick en tiempo real es eventual (hasta 5s); el bloqueo real
  es en el proximo intento de acceso/upgrade.

### 5.5 Cliente

- Server settings > Members: listar miembros, kick/ban buttons (owner)
- Server settings > Bans: listar, unban
- User context menu: block, report
- Mensaje context menu: report
- Server settings > Transfer: selector de miembro (owner)

## Aceptacion

- [ ] Ban -> el user no puede hacer join por invite (403)
- [ ] Ban -> WS upgrade a channels del server -> 403
- [ ] Unban -> join funciona de nuevo
- [ ] Block -> no recibe DM ni friend request del bloqueado
- [ ] Report -> fila en reports
- [ ] Transfer -> nuevo owner puede gestionar; viejo ya no
- [ ] Server huerfano -> miembro puede transferir
- [ ] Budget: writes por ban/block/report = 1 fila cada uno (despreciable)

## Riesgos

| Riesgo | Mitigacion |
|---|---|
| Kick no es instantaneo en WS activos | Aceptado: acceso bloqueado en proximo intento; broadcast de aviso |
| Block vs mensajes en channels compartidos | v1: block afecta solo DMs/friend requests. Filtrado de mensajes en shared channels = Fase 6 |
| Abuse de reports | Rate limit 5/dia por user en POST /api/reports |
