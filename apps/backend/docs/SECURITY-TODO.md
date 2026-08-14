# TODO — Sanitización server-side de contenido de chat

**Estado:** PENDIENTE. Documento de trabajo; el cliente Slint hoy renderiza el
contenido como texto plano (`TextInput read-only`), por lo que no hay XSS
clásico en el render actual. Este TODO cubre la defensa server-side que el
patrón OWASP recomienda como capa de almacenado, para cuando el protocolo o el
cliente permitan rich text / markdown (p. ej. `StyledText` con `@markdown`).

## Riesgo que se mitiga

- **Inyección de markup / phishing visual:** si el contenido se renderiza
  con `StyledText` (CommonMark + `<u>`/`<font color>`/links), un usuario
  podría inyectar estilos, links falsos o texto que se vea como UI legítima.
- **Almacenado de contenido hostil:** el patrón OWASP manda sanitizar antes
  de persistir, no confiar en que "el cliente nunca interpreta markup".

## Punto de intervención exacto

`apps/backend/src/index.ts` → `POST /api/channels/:id/messages` (y el mismo
path para DMs si aplica). Hoy:

```ts
const body = await readJson(ctx.request);
if (!validateContent(body.content)) {
  throw new ApiError(422, "invalid_content", ...);
}
```

- `validateContent` solo chequea largo (1–2000 chars). No hay sanitización.
- El contenido se guarda tal cual en D1 (`db.insertMessage(...)`).

## Trabajo a hacer

1. **Decidir el modelo de contenido.** Dos opciones (elegir UNA y documentar):
   - **Plano (recomendado hoy):** el contenido sigue siendo texto plano;
     la sanitización se reduce a control de caracteres de control y largo.
     Nada que parsear.
   - **Rich text estructurado:** si se adopta markdown, definir un subconjunto
     permitido (allowlist) y un serializador determinista — nunca guardar HTML
     crudo del cliente.

2. **Si se adopta markdown (futuro):** sanitizar en el servidor con una
   allowlist explícita (sin DOMPurify en Cloudflare Workers; evaluar
   `@gitlab-org/security-markdown` o un parser propio restringido):
   - Solo un subconjunto de tags/entidades permitidas (negrita, itálica,
     código inline, links http/https).
   - Links: solo scheme `http`/`https`; **rechazar userinfo**
     (`https://user:pass@host`), punycode engañoso y caracteres de control.
   - Escapar todo lo demás como texto literal.
   - Aplicar el mismo normalizador en el cliente para consistencia.

3. **Sanitización de display de URLs (cliente, ya parcial):**
   - `apps/lumen-slint/src/model.rs::detect_link` ya rechaza userinfo,
     caracteres de control y hosts vacíos (ver `detect_link_tests`).
   - El preview de link ya está protegido contra SSRF en
     `apps/lumen-slint/src/controller.rs::is_public_ip` (bloquea IPs
     privadas/loopback/link-local/metadata, redirects limitados, body cap).

## Criterio de aceptación

- `POST /api/channels/:id/messages` no persiste contenido que pueda
  interpretarse como markup hostil.
- El modelo de contenido elegido está documentado (plano vs rich text).
- Tests del sanitizer: texto plano, markup inyectado, links con userinfo,
  caracteres de control, largo máximo.

## Notas

- El cliente usa `StyledText` para *otras* superficies (ver `ui/`), pero el
  body de mensajes es `TextInput read-only` — verificar que siga así si se
  añade rich text.
- Mantener en lockstep con `packages/protocol` (esquema de `TextMessage`).
