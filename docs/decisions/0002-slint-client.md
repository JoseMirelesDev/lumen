# ADR 0002 — Migración del cliente a Slint

Status: accepted · Date: 2026-08-05 · Branch: `slint-client`

## Context

El cliente desktop (Tauri 2 + Svelte 5 en WebView) carga ~313 MB RSS en idle
(127 MB main + 144 MB WebKitWebProcess + 47 MB WebKitNetworkProcess) y depende
de la cadena Node/pnpm + `webkit2gtk` + un patch vendored de `wry`. La voz ya
es nativa en Rust (`cpal → opus → webrtc-rs`), fuera del WebView, con
`audio.rs`/`signaling.rs`/`rtp.rs` libres de acoplamiento a Tauri; el único
acoplamiento son 9 puntos donde `AppHandle` viaja por `client.rs` para emitir
7 eventos `voice://`, y 4 comandos IPC mediados por `tauri::State`.

Decisión: reemplazar el frontend WebView por **Slint** (UI nativa Rust,
declarativa, multiplataforma), manteniendo el backend Cloudflare y el protocolo
intactos. Migración incremental en la rama `slint-client`.

## Decisions

### UI: Slint 1.17 (Rust, `.slint`)
- Compila a código nativo; sin runtime JS, sin WebKit. Renderer Skia (GPU) con
  fallback a software (`SLINT_BACKEND` en runtime) — relevante para la HD 4600
  de esta máquina, que hoy exige `WEBKIT_DISABLE_COMPOSITING_MODE=1`.
- Dark-only, réplica 1:1 de la identidad actual (paleta de 10 tokens + hardcoded
  de `app.css` → `global Theme` en Slint).
- Se usan solo features estables (1.16+): `KeyBinding`/`@keys`, `StyledText` se
  evalúan en Fase 5+; no se depende de features experimentales (FlexboxLayout,
  Drag&Drop, Library modules) salvo reevaluación explícita.

### Arquitectura: workspaces de crates + capas
```
crates/lumen-protocol    tipos REST/WS en Rust (serde) — fuente de verdad TS, generado
crates/lumen-core        state, ApiClient (REST+WS), auth, EventBus, PluginManager
crates/lumen-voice       cliente de voz desacoplado de Tauri (media, sin UI)
crates/lumen-plugin-api  trait Plugin + manifiesto + capacidades (estable, versionada)
apps/desktop             binario Slint (reemplaza src-tauri + src)
```
- Dependencia unidireccional: UI → Controller → Core → Protocolo.
- Raíz: se añade `Cargo.toml` workspace (hoy solo existe el workspace pnpm).

### Contrato de voz (el pendiente): core agnóstico + adaptadores
- `lumen-voice` reemplaza `AppHandle` por `mpsc::UnboundedSender<VoiceEvent>`.
- `VoiceEvent` enum tipado que refleja 1:1 los 7 canales actuales
  (`levels`, `debug`, `peer-joined`, `peer-left`, `state`, `signaling`, `error`).
- Los 4 comandos (`voice_join/leave/set_muted/set_deafened`) pasan a métodos
  directos del `VoiceClient`, sin IPC.
- **Adaptadores en el host, no en el core**: `TauriAdapter` reemite `VoiceEvent`
  con los payloads JSON exactos actuales → la app Svelte sigue funcionando sin
  cambios durante la transición; `SlintAdapter` (Fase 4) alimenta la UI vía
  `invoke_from_event_loop`. El contrato se congela y se valida con tests antes
  de tocar la UI.

### Protocolo: single source of truth en TS (backend), Rust generado
- `packages/protocol` (TS) sigue siendo la fuente; el Worker es TS.
- Spike en Fase 1: **typeshare** para emitir `lumen-protocol` (serde con tags
  `kebab-case` + campos `camelCase`). Si typeshare no cubre el wire format
  exacto → fallback: tipos Rust manuales + golden tests contra fixtures TS.

### Plugins/addons: registro + capacidades, motor Rhai detrás de feature flag
- `lumen-plugin-api` con `API_VERSION`, manifiesto `lumen-plugin.toml`
  (name/version/api_version/permissions) y modelo de capacidades.
- Motor por defecto: **Rhai** (embebido, sin FFI → sandbox por diseño, hot-reload,
  DX alta). Opcional nativo: `libloading`+C ABI. V2 si llegan plugins de terceros
  no confiables: **wasmtime** detrás de la MISMA `PluginApi` (el trait no cambia).
- La UI de plugins es data-driven: los plugins registran comandos/paneles en
  modelos; Slint no carga `.slint` en runtime (se compila en build-time), así que
  el host renderiza extensiones genéricas, el binario queda cerrado.

### Empaquetado y CI (Fase 6)
- Se elimina la cadena Node/pnpm del cliente; quedan deps nativas ya presentes:
  ALSA, meson/ninja, CMake, libclang. Desaparecen `webkit2gtk`, `wry` vendored.
- Empaquetado con **cargo-dist** (tarball/.deb/.rpm/.msi/.dmg); AppImage vía paso
  extra (linuxdeploy) si se requiere. Decisión final en Fase 6.

## Consequences

- Reducción esperada de RSS idle (~313 MB → objetivo 40-80 MB) y arranque más
  rápido; sin cambios en la ruta de media (ya nativa).
- La UI Svelte y `src-tauri` se eliminan al corte (Fase 6); durante la migración
  conviven en la rama.
- Riesgos: render de emoji (font), wire format de typeshare, madurez desktop de
  Slint (1.17 en push desktop-ready). Ver sección Riesgos del plan.
