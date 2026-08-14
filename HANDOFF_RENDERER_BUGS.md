# Handoff: bugs de partial rendering en el fork de Slint (ghost en scroll + animación del fuego)

Rol: ingeniero de sistemas con experiencia en renderers. Repo: `/home/reny/Projects/discord-light`.
App: `apps/lumen-slint` (package `lumen-desktop`). Solo tocás: `vendor/slint-core/`, `vendor/slint-renderer-skia/`, `vendor/slint-backend-winit/`, y si hace falta `apps/lumen-slint/src/` y `apps/lumen-slint/ui/`. NO toques `crates/lumen-voice/` ni `crates/lumen-core/`.

## Contexto

La app usa un **fork vendored de Slint** (dentro del repo, rama `slint-client`) con **partial rendering a medida**:
- `main.rs` fuerza `SLINT_SKIA_PARTIAL_RENDERING=1` antes de crear la ventana.
- El renderer `vendor/slint-renderer-skia/lib.rs` + `vendor/slint-core/partial_renderer.rs` repintan solo las dirty regions por frame.
- **Mecanismo fork (clave):** `properties_changed_since_render` (bool en `SkiaRenderer`) se setea desde el hook `RendererSealed::property_changed()`, llamado por `WindowRedrawTracker::notify` (`vendor/slint-core/window.rs:423`). En el render loop (`lib.rs` ~750):
  ```rust
  partial_rendering_state.skip_item_dirty_compute.set(
      !self.properties_changed_since_render.replace(false),
  );
  ```
  Si ninguna propiedad cambió desde el último render, **se saltea el traversal por-item** (`compute_dirty_regions` en `partial_renderer.rs`) y se repinta solo: las regiones marcadas externamente (`mark_dirty_region`, de la animación) + el historial buffer-age.
- **Animación del fuego:** el push de voz (~10 Hz, `voice.rs push()`) → `particles::tick_fire` → `window.set_animation_frame(1, i)` → `SkiaRenderer::set_animation_frame` (`lib.rs:1091`) → `animation_registry.set_frame` (`animation.rs:479`) → rect diff desde `slot.last_rect` → `mark_dirty_region(rect)` + `request_redraw`. `last_rect` se graba en `draw_animation_image` (`itemrenderer.rs:683`).
- El skip existe para que el tick del fuego no re-renderice toda la escena (sin él, un core se pincha 20-25% por los renders completos).

## Bugs a arreglar

### Bug A — ghost al scrollear (Settings modal)
Scrollear el `ScrollView` de Ajustes deja **fantasmas/píxeles duplicados** del contenido que salió de vista. Solo pasa al scrollear.

**Evidencia** (con `SLINT_SKIA_PARTIAL_RENDERING=log`): durante el scroll el repintado se queda en ~19% (la región del fuego) — **el contenido scrolleado nunca se invalida**.

**Hipótesis de causa raíz (fuerte, a verificar):** tras un render con skip (tick del fuego), el `redraw_tracker` del window (`window.rs:487`, evaluado en `draw_contents` `window.rs:1563` vía `evaluate_as_dependency_root`) **reconstruye sus dependencias solo con los items realmente dibujados** (la región marcada) → `viewport-y` del Flickable deja de ser dependencia → al scrollear, `WindowRedrawTracker::notify` **no dispara** → `properties_changed_since_render` queda `false` → el render del scroll **saltea el traversal** → el contenido movido no se marca dirty → los píxeles viejos persisten.

**Cuidado:** verificar que el skip sea realmente la causa (comparar el comportamiento de `properties_changed_since_render` antes/después de un render con skip). Probar también el roster de la sala de voz (tiene otro ScrollView).

### Bug B — animación del fuego congelada (apareció al intentar arreglar A)
Un intento anterior **quitó el skip** (siempre correr el traversal) para arreglar A. Resultado: el scroll se arregló pero **el fuego dejó de animar** (frame estático). El intento fue **revertido** (el skip está restaurado; el estado actual: fuego anima, ghost presente).

**Evidencia del intento fallido** (con debug `LUMEN_DEBUG_FIRE`, ya removido): los ticks y el mark ocurren (`set_frame → mark Rect(...)`) pero el render inmediato tomaba `force_dirty` **vacío** (`[render] initial dirty rects: 0` — instrumentado en `create_partial_renderer`). O sea: con el traversal siempre activo, **el mark de `set_animation_frame` no llegaba a la dirty region del render**. NO está root-causeado. Posibles vías: coalescing del `request_redraw` en el backend winit, orden mark/take, o que existan dos instancias de `PartialRenderingState` (mark en una, render en otra — verificar cuántas veces se crea y si `set_surface`/`reset_storage` reemplaza el estado).

## Dirección esperada del fix correcto

1. **Bug A:** el skip no debe podar el grafo de dependencias del `redraw_tracker`. Opciones a evaluar (con medición, no a ciegas):
   - (a) Cuando hay skip, correr el traversal en modo "solo registro de dependencias" (sin el costo de evaluar geometría/marcar dirty) para que `viewport-y` siga siendo dependencia.
   - (b) Mantener el skip pero forzar un traversal completo periódico (cada N frames o tras input).
   - (c) Corregir la decisión de skip para que también considere input pendiente/scroll reciente.
2. **Bug B:** investigar y arreglar por qué el mark de la animación no llega al render cuando el traversal corre. Si resulta ser el mismo mecanismo (deps/registro), el fix de A podría cubrir ambos.
3. **Restricciones duras:**
   - El fuego debe animar (conectado a un canal de voz).
   - El scroll de Settings no debe dejar fantasmas.
   - El costo del tick del fuego debe seguir bajo (~0.6% de un core es el ahorro del skip; full-scene por tick = 20-25% de un core, inaceptable).
   - Los toggles de Settings → VISUALES ("Animación de la fogata", "Partículas de la fogata") y el comportamiento de visibilidad del fuego (visible con partículas ON aunque animación OFF — frame congelado) deben seguir funcionando.

## Reproducción y verificación

- Build: `cargo build -p lumen-desktop --release` → binario `target/release/lumen`.
- Run: `target/release/lumen` (X11, el usuario prueba interactivamente — no hay driver de UI disponible).
- Diagnóstico de repintado: lanzar con env `SLINT_SKIA_PARTIAL_RENDERING=log` (imprime `repainting X%` por frame). Un scroll correcto debe mostrar repintados grandes (~el viewport); los ticks del fuego ~13-19%.
- Si necesitás instrumentar el fuego: agregar `eprintln!` gateados por `LUMEN_DEBUG_FIRE` (en `set_animation_frame` y en el render, justo tras `create_partial_renderer` — se removieron al revertir).
- Aceptación:
  1. Scroll en Ajustes sin fantasmas (y en el roster de la sala).
  2. Fuego animando conectado a un canal de voz.
  3. Tick del fuego barato (con `SLINT_SKIA_PARTIAL_RENDERING=log`, el patrón del tick sigue siendo ~13-19% de repintado, no 100%).
  4. Toggles VISUALES funcionando como antes.

## Archivos clave

- `vendor/slint-renderer-skia/lib.rs` — render loop, `skip_item_dirty_compute.set(...)` (~750), `set_animation_frame` (~1091), `mark_dirty_region`, `property_changed` (campo `properties_changed_since_render` ~186, restaurado).
- `vendor/slint-core/partial_renderer.rs` — `compute_dirty_regions` (~385), `apply_dirty_region` (~795), `create_partial_renderer` (~784, hace `force_dirty.take()`).
- `vendor/slint-core/window.rs` — `WindowRedrawTracker::notify` (~423), `redraw_tracker` (~487), `draw_contents` con `evaluate_as_dependency_root` (~1563).
- `vendor/slint-renderer-skia/animation.rs` — `set_frame` (~479), `last_rect`.
- `vendor/slint-renderer-skia/itemrenderer.rs` — `draw_animation_image` (~683).
- `apps/lumen-slint/src/voice.rs` — push (~440-460): gate del tick (`!ui.get_reduced_motion() && particles_enabled`).
- `apps/lumen-slint/ui/voice-view.slint` — el `AnimationImage` del fuego (`if root.connected && root.particles-enabled`).

## Estado actual del código (post-revert)

- Skip restaurado (Bug A presente, Bug B ausente): fuego anima, ghost en scroll.
- Toggles VISUALES + visibilidad desacoplada del fuego: funcionando.
- Sin debug prints (se removieron).
