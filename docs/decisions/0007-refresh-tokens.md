# ADR 0007 — Refresh tokens con rotación

Status: proposed · Date: 2026-08-13 · Scope: `plans/backend-v2` Fase 1

## Context

El JWT actual dura 7 días y no es revocable: un token robado es válido una
semana, y no hay forma de cerrar sesiones. Un access token largo en un
cliente nativo es un riesgo de exfiltración.

## Decision

Dos tokens: **access** (JWT HMAC, TTL 1h, solo memoria del cliente) y
**refresh** (opaco, TTL 30 días, persistido en settings.json, hash SHA-256
en D1 con revocación). `POST /api/auth/refresh` rota: revoca el viejo y
emite uno nuevo (reuso = 401). Logout revoca. `DELETE /api/auth/sessions`
revoca todos del usuario. El WS upgrade valida access; la sesión WS vive
mientras el socket viva.

## Alternatives considered

- **JWT largo (7d) sin revocación**: cero infra, pero robo = 7 días de
  acceso y sin logout real. Rechazado (es el estado actual).
- **Sessions server-side (tabla por sesión, cookie/header id)**: más estado
  en D1 (1 write por login, reads por request), sin beneficio sobre refresh
  con rotación para un cliente nativo. Rechazado.
- **OAuth/OIDC completo (Authorization Code + PKCE) para primer login**:
  es ADR para Fase 4 (OAuth social); el auth interno sigue con refresh.

## Consequences

- Positivo: revocación real, ventana de robo = 1h, logout funcional,
  rotación detecta reuso (token compromise signal).
- Negativo: el cliente debe persistir el refresh token y manejar 401→refresh
  (complejidad en api.rs); +1 tabla D1; +1 write por login + 1 por rotación.
- Riesgo: refresh token en disco (settings.json) — mitigado: es opaco,
  revocable, y la rotación limita la ventana.
- Reversibilidad: alta — los endpoints son aditivos; el JWT access mantiene
  el mismo formato.

## Revisit when

- Se agregue un web client (necesita cookies httpOnly + CSRF, mismo modelo
  de rotación)
- Se requiera 2FA (los refresh tokens son el hook natural para step-up auth)
