# Revisión: bugs de partial rendering en el fork de Slint (ghost en scroll + animación del fuego)

## Rol

Ingeniero de sistemas senior con experiencia en renderers (Skia, compositing, dependency tracking de propiedades reactivas). Revisar el problema end-to-end, validar o refutar los hallazgos, y proponer el fix definitivo. NO modificar `crates/lumen-voice/` ni `crates/lumen-core/`. Solo lectura/análisis salvo que se pida lo contrario.

## Contexto

App `apps/lumen-slint` (package `lumen-desktop`, binario `lumen`) usa un **fork vendored de Slint** con **partial rendering a medida** (rama `slint-client`):

- `main.rs` fuerza `SLINT_SKIA_PARTIAL_RENDERING=1` antes de crear la ventana.
- El renderer `vendor/slint-renderer-skia/` + `vendor/slint-core/partial_renderer.rs` repintan solo dirty regions por frame.
- **Mecanismo fork (clave):** `properties_changed_since_render` (bool en `SkiaRenderer`) se setea desde `RendererSealed::property_changed()`, llamado por `WindowRedrawTracker::notify` (`vendor/slint-core/window.rs:424`). En el render loop (`lib.rs:704`):
  ```rust
  let properties_changed = self.properties_changed_since_render.replace(false);
  partial_rendering_state.skip_item_dirty_compute.set(!properties_changed && !force_traversal);
  if skip { partial_rendering_state.register_dependencies(components); }
  ```
  Si ninguna propiedad cambió desde el último render, se saltea el traversal por-item (`compute_dirty_regions` en `partial_renderer.rs:385`) y se repinta solo: regiones marcadas externamente (`mark_dirty_region`, de la animación del fuego) + historial buffer-age.
- **Animación del fuego:** push de voz (~10-12 Hz, `voice.rs`) → `particles::tick_fire` → `window.set_animation_frame(1, i)` → `SkiaRenderer::set_animation_frame` (`lib.rs:1153`) → `animation_registry.set_frame` (`animation.rs:479`) → rect diff desde `slot.last_rect` → `mark_dirty_region(rect)` + `request_redraw`. `last_rect` se graba en `draw_animation_image` (`itemrenderer.rs:683`).
- El skip existe para que el tick del fuego no re-renderice toda la escena (sin él: 20-25% de un core; con skip: ~0.6%).

## Bugs reportados originalmente

### Bug A — ghost al scrollear (Settings modal)
Scrollear el `ScrollView` de Ajustes deja fantasmas/píxeles duplicados del contenido que salió de vista. Evidencia previa (con `SLINT_SKIA_PARTIAL_RENDERING=log`): durante el scroll el repintado se quedaba en ~19% (región del fuego) — el contenido scrolleado nunca se invalidaba.

### Bug B — animación del fuego congelada
Un intento previo de arreglar A quitó el skip; resultado: scroll OK pero fuego estático. Fue revertido.

## Hallazgos actuales (verificar/criticar)

### 1. Root cause de Bug B (CONFIRMADO por bytes crudos + corridas)
En `lib.rs:1153` `set_animation_frame`, la llamada al mark estaba **comentada por accidente** (línea fusionada con un comentario):
```rust
// Repaint only the item's rectangle; the rest of the scene tree is not
// re-evaluated (the item's rendering tracker is clean).        self.mark_dirty_region(rect.into());
```
Toda la línea es un comentario → `mark_dirty_region` nunca se ejecutaba. Evidencia: 142 `set_frame` con `mark Rect(...)` impresos (solo debug print) pero **0 llamadas a `mark_dirty_region`**; `force_dirty` siempre vacío; renders con skip pintando `-0.00%`. Esto explica el "force_dirty vacío" del intento fallido: el "mark" que veían era el print, no la llamada real.
**Estado: restaurado** (la llamada ahora ejecuta). Tras el fix: 142 marks → 142 llamadas; los ticks repintan 12-18%.

### 2. Root cause de Bug A (CONFIRMADO por análisis + corridas)
`PropertyTracker::evaluate_as_dependency_root` (`vendor/slint-core/properties.rs:1313`) **borra y reconstruye los deps del `redraw_tracker` en cada evaluación**. El render corre dentro de `draw_contents` → `redraw_tracker.evaluate_as_dependency_root(|| render_components(...))` (`window.rs:1563`). En un render con skip, solo se dibujan los items de la región marcada → el redraw_tracker queda dependiendo SOLO de esos items (el AnimationImage + props del fuego). El `viewport-y` del Flickable deja de ser dependencia → al scrollear, el cambio de viewport-y NO marca el redraw_tracker → `WindowRedrawTracker::notify` no dispara → `properties_changed_since_render` queda false → el render del scroll saltea el traversal → el contenido movido no se marca dirty → píxeles viejos persisten (ghost).
Mecánica de la dependencia: el compilador crea un elemento "viewport" fake como hijo del Flickable con `y <=> viewport-y` (two-way) (`vendor/slint-compiler/passes/flickable.rs`, `create_viewport_element`); el traversal lee `item_rc.geometry()` de cada item (tracked) → registra las props de geometría (incl. viewport-y vía la two-way) como deps del redraw_tracker. Con el skip, esas lecturas no ocurren.
Nota: `Property::set` (`properties.rs:1005`) solo marca dirty si el valor CAMBIA → con nivel de voz constante, el skip se activa (estado estable); con audio activo, los renders son full-traversal y los deps se reconstruyen (por eso el ghost aparecía sobre todo con audio estable).

**Fix implementado:** pase `register_dependencies` (`partial_renderer.rs:852`) que corre en los frames con skip: visita todos los items, lee `item_rc.geometry()` (tracked → registra deps) y re-registra los rendering trackers existentes (`register_as_dependency_to_current_binding`), SIN marcar dirty ni mutar la cache. Costo medido: ~120-200µs/frame (a 12.5 Hz ≈ 0.15-0.25% de un core).
**Evidencia post-fix (corrida automatizada):** durante el scroll, 57 `property_changed` + 57 renders con traversal (antes: 0).

### 3. NUEVO hallazgo — el modal de Ajustes apenas scrollea (~5px de rango)
Medido en el ScrollView de Ajustes: flickable `geom=(372,383)`, `viewport-height=388` → el contenido es solo **5px más alto** que el viewport → rango de scroll `[-5, 0]` → el modal prácticamente no scrollea en la ventana completa (1100x720, scale 1.0, card 420x480). `ensure_in_bound` clampea todo a ±5px. En ventana más chica (card clampeada a `min(480, h-24)`), el overflow crece (~100px a 600x400) y el scroll es real.
- Esto es LAYOUT (`apps/lumen-slint/ui/settings.slint`, `theme.slint` — card-height 480px), NO del renderer. No lo tocamos.
- Hipótesis: el ghost original se observó con más overflow (más secciones en ese momento, o ventana más chica). Verificar cuál es la reproducción real del usuario.
- El roster de la sala de voz (`voice-view.slint`) tiene otro ScrollView con overflow potencialmente real (depende de peers).

### 4. Artefacto de la corrida automatizada (run4)
Mi driver de test (`rendertest.rs`) despachaba wheel events (delta +40) cada 60ms DURANTE la prueba manual del usuario: sus eventos reales (delta -60, física) peleaban contra los míos — el viewport oscilaba entre 0 y -2.5px. Interferencia del harness, no del app. No repetir pruebas automatizadas mientras el usuario prueba.

## Qué se necesita determinar

1. **¿Cuál es la reproducción REAL del usuario?** (a) ghost al scrollear con overflow real (¿roster? ¿ventana chica?), (b) "no scrollea" por el rango de 5px, (c) ambas. El usuario reporta "el scroll sigue fallando".
2. **¿El fix de deps (register_dependencies) es correcto y suficiente?** Revisar:
   - ¿Cubre TODOS los casos de poda? (geometría + rendering trackers + `children_transform` de items con Transform/Rotate — el traversal también registra esos deps en `CachedItemBoundingBoxAndTransform::new`).
   - ¿Interfiere con el flujo del modal (open/close, scrollbar, toggles)?
   - ¿Hay fugas de registros duplicados en `dep_nodes` si el pase corre repetidamente? (`register_self_as_dependency` hace push de un nodo nuevo por llamada; `evaluate_as_dependency_root` los libera en la próxima evaluación — verificar que no crezca sin bound).
   - ¿El pase debe correr también para los popups activos (`active_popups` ChildWindow)? `draw_contents` pasa la lista completa de componentes al renderer.
3. **El skip en sí**: ¿es la decisión correcta? Alternativas a evaluar: (a) pase de deps (implementado), (b) traversal "solo deps" integrado en `apply_dirty_region` con un flag, (c) no saltar si hay input reciente, (d) forzar traversal periódico. Medir costos.
4. **El mark del fuego con el modal abierto**: la región del fuego (diff rect ~600x190) queda DETRÁS del modal (backdrop translúcido) — el renderer la repinta igual (12-19%). ¿Correcto? ¿Interfiere con el repintado del contenido del modal?
5. **Roster**: ¿el mismo mecanismo aplica? ¿Hay que poblar peers para probarlo?

## Archivos a revisar (con líneas clave actuales)

### Renderer Skia
- `vendor/slint-renderer-skia/lib.rs`
  - `render_components_to_canvas` ~704 (render loop; decisión de skip ~757-782; `register_dependencies` call ~771; instrumentación `LUMEN_DEBUG_RENDER`)
  - `mark_dirty_region` ~1129, `property_changed` ~1138, `set_animation_frame` ~1153 (mark restaurado)
  - `partial_rendering_state()` ~925
  - `take_snapshot` ~1095 (nota: consume `force_dirty` — artefacto del harness)
- `vendor/slint-renderer-skia/animation.rs` — `AnimationSlot` (current/uploaded/last_rect), `set_frame` ~479, `register` (~400), `update_surface` (~330), `upload_current`
- `vendor/slint-renderer-skia/itemrenderer.rs` — `draw_animation_image` ~683 (`record_rect`), `draw_image_impl`
- `vendor/slint-renderer-skia/README.md` y `docs/performance-fork-design.md` (si existe; diseño original del fork)

### Core Slint
- `vendor/slint-core/partial_renderer.rs` — `compute_dirty_regions` ~385 (traversal completo; registra deps de geometría + trackers), `create_partial_renderer` ~784 (`force_dirty.take()`), `apply_dirty_region` ~795 (skip + buffer-age), **`register_dependencies` ~852 (NUEVO pase)**, `mark_dirty_region` ~887, `CachedItemBoundingBoxAndTransform::new` ~120, `PartialRenderer` (filter_item/do_rendering)
- `vendor/slint-core/window.rs` — `WindowRedrawTracker::notify` ~424 (property_changed + request_redraw), `redraw_tracker` ~487, `draw_contents` ~1551-1600 (`evaluate_as_dependency_root`)
- `vendor/slint-core/properties.rs` — `PropertyTracker::evaluate_as_dependency_root` ~1313 (borra deps), `Property::set` ~1005 (solo dirtys si cambia el valor), `register_as_dependency_to_current_binding` ~764/1277, `register_self_as_dependency` ~461, `mark_dependencies_dirty` ~827, dependency_tracker (dep_nodes)
- `vendor/slint-core/items/flickable.rs` — `process_wheel_event` ~441 (instrumentación temporal `[lumen-wheel]`), `is_allowed_scroll_direction`, `viewport_x/y` ~62
- `vendor/slint-core/item_rendering.rs` — `render_item_children` ~191 (filter_item + translate), `render_component_items`
- `vendor/slint-core/item_tree.rs` — `visit_items` ~1302, `ItemRc::geometry` ~573
- `vendor/slint-core/renderer.rs` — trait `Renderer` (mark_dirty_region, property_changed, set_animation_frame ~110-121)

### Backend winit
- `vendor/slint-backend-winit/winitwindowadapter.rs` — `request_redraw` ~1282 (throttle), `draw` ~691 (pending_redraw=false antes del render)
- `vendor/slint-backend-winit/frame_throttle.rs` — TimerBasedFrameThrottle (fix previo del doble render)
- `vendor/slint-backend-winit/event_loop.rs` — `RedrawRequested` ~218, `about_to_wait` ~614 (has_active_animations → request_redraw)

### Compiler (cómo se generan las geometrías)
- `vendor/slint-compiler/passes/flickable.rs` — `create_viewport_element` (~46): fake viewport con `y <=> viewport-y`; ListView: `y = actual-y - viewport-y`
- `vendor/slint-compiler/generator/rust.rs` — `item_geometry` ~1732 (match por índice → expresión de geometría; lecturas tracked)

### App
- `apps/lumen-slint/src/main.rs` — env `SLINT_SKIA_PARTIAL_RENDERING`, `LUMEN_AUTOJOIN`, hook `rendertest::maybe_run`
- `apps/lumen-slint/src/rendertest.rs` — harness de test automatizado (artefacto: take_snapshot consume force_dirty; no correr mientras el usuario prueba)
- `apps/lumen-slint/src/voice.rs` — `push` ~440-478 (gate del tick: `!reduced_motion && particles`)
- `apps/lumen-slint/src/particles.rs` — `tick_fire` ~349, `init` (registro de frames)
- `apps/lumen-slint/ui/settings.slint` — el modal (ScrollView con contenido ~388px vs viewport 383px → rango ~5px)
- `apps/lumen-slint/ui/voice-view.slint` — `AnimationImage` ~348-356 (`if root.connected && root.particles-enabled`), roster (ScrollView de peers)
- `apps/lumen-slint/ui/app.slint` — `overlay`, `voice-visible`, `reduced-motion`, `voice-particles-enabled` (~83-234)
- `apps/lumen-slint/ui/primitives/overlay.slint` — VoxOverlay (backdrop, card clampeada)

## Reproducción y verificación

- Build: `cargo build -p lumen-desktop --release` → `target/release/lumen`
- Run (X11, display :0 — el usuario prueba interactivamente; NO correr tests automatizados que abran ventanas mientras él prueba):
  - `SLINT_SKIA_PARTIAL_RENDERING=log target/release/lumen` → imprime `repainting X%` por frame (ticks del fuego ~12-19%; scroll correcto debe mostrar repintados grandes del contenido movido).
  - Agregar `LUMEN_DEBUG_RENDER=1` → imprime por frame: `props_changed`, `skip`, `initial_dirty`, `frame_region/painted`, `traversal` (µs), `deps_pass` (µs), `property_changed`, `mark_dirty_region`, `[lumen-wheel]` (instrumentación temporal en flickable.rs).
  - `LUMEN_TEST_NO_SKIP=1` → fuerza traversal siempre (config del intento fallido; útil para A/B).
  - `LUMEN_AUTOJOIN=<channel_id>` → auto-join a un canal de voz (requiere backend vivo + token persistido).
- Harness automatizado (solo sin el usuario en la máquina): `LUMEN_RENDER_TEST=1` → fuerza estado (logged-in, voz activa, settings abierto), ticks de fuego 80ms, wheel scripted en la card, snapshots PPM a `/tmp/lumen-test/`.
- Criterios de aceptación: (1) scroll sin ghosts (Settings + roster), (2) fuego animando conectado a voz, (3) tick del fuego barato (~12-19% repaint, deps_pass ≤ ~200µs), (4) toggles VISUALES ("Animación de la fogata", "Partículas de la fogata") y visibilidad desacoplada (fuego visible con partículas ON aunque animación OFF) funcionando.

## Logs de referencia (evidencia ya capturada)

- `/tmp/lumen-test/run.log` — build original (mark muerto): 117 set_frames, 0 mark_dirty_region, skip activo, painted -0.00% en skip.
- `/tmp/lumen-test/run2.log` — post-fix mark: 142 marks → 142 mark_dirty_region, deps_pass 120-200µs, repaint 12-18% por tick.
- `/tmp/lumen-test/run3.log` — scroll con wheel en posición correcta: 57 property_changed + traversal durante scroll.
- `/tmp/lumen-test/run4.log` — con instrumentación `[lumen-wheel]`: eventos del driver (pos 186,39 / 234,191, delta +40) peleando con input real del usuario (delta ±60, físicas); settings flickable geom 372x383, viewport-height 388 → rango [-5,0].
- Snapshots: `/tmp/lumen-test/*.ppm` (PPM 1100x720; card 420x480 centrada).
