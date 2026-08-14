# Diseño: componente de animación aislada (fork mínimo de Slint)

**Problema:** rotar el `source` de una `Image` en Slint marca dirty su rect → el partial
renderer lo incluye en la región a repintar → se re-renderiza el subárbol (traversal +
bindings + comandos GL + allocs). Medido: ~9.3 puntos de un núcleo al rotar 12.5Hz, y
reducir el área (Image chica) solo ahorra ~2.6. **El acto de rotar propaga el dirty al
árbol.**

**Objetivo:** un componente cuyo contenido animado cambie **sin ensuciar ni propagar** al
resto del árbol — que viva en su propia textura y solo se blitee.

## Cómo funciona el mecanismo actual (verificado en el source)

```
Layer (cache-rendering-hint)
  └─ cache.get_or_update_cache_entry(item)     ← item_rendering.rs:665
       ├─ cache válido (nada cambió) → blit imagen cacheada
       └─ cache inválido (binding cambió) → re-renderiza subárbol a textura
```

El cache se invalida cuando **cualquier binding dentro del subárbol cambia**. Para el
fuego, cambiar el `source` de la Image interna invalida → re-renderiza el subárbol →
propaga.

**El insight:** si el contenido animado está **FUERA del árbol reactivo** (una textura
que cambia por Rust, sin que ningún binding de Slint cambie), el cache del Layer no se
invalida — pero entonces Slint blitearía la imagen VIEJA. **Falta un hook para decirle
al cache "la textura externa cambió, actualizá tu imagen, pero NO toques el árbol".**

## El cambio mínimo: exponer `mark_dirty_region` + un item "animation"

### 1. Exponer `mark_dirty_region` como API pública

Ya existe interno en `RendererSealed` (`i-slint-renderer-skia/lib.rs:1037`):

```rust
// HOY (interno, sealed)
fn mark_dirty_region(&self, region: DirtyRegion) {
    if let Some(partial_rendering_state) = self.partial_rendering_state() {
        partial_rendering_state.mark_dirty_region(region);
    }
}
```

**Cambio:** agregar una variante pública en `slint::Window`:

```rust
// PROPUESTA
pub fn mark_dirty_region(&self, rect: LogicalRect) {
    let region = DirtyRegion::Rect(rect.to_physical(self.scale_factor()));
    self.0.renderer().mark_dirty_region(region);
}
```

Esto le dice al partial renderer: "repintá SOLO este rect". **No recorre el árbol, no
invalida bindings, no propaga** — solo agrega el rect a la región sucia del próximo frame.

### 2. Item "animation" — un Image cuyo contenido se actualiza por fuera

Propiedad nueva en el item `Image` (o un item derivado):

```slint
// PROPUESTA — en .slint
Image {
    animation-texture: root.fire-texture;  // ID de textura GL (u64)
    animation-frame: root.fire-frame;      // índice de frame (int)
}
```

**Semántica:** cuando `animation-frame` cambia (seteado desde Rust con un timer),
el renderer:
1. **No** invalida el árbol (el binding de `animation-frame` se marca como "externo",
   sin dependencias rastreadas)
2. Actualiza el contenido del rect a partir de la textura GL (ya subida por Rust con
   tiny-skia) — un blit de región
3. Llama `mark_dirty_region(rect)` para que el próximo frame repinte solo ese rect

### 3. Flujo completo (cómo se usa)

```mermaid
flowchart LR
    subgraph Rust (fuera del árbol)
        A[tiny-skia hornea 60 frames<br/>1 vez al arrancar] -->|glTexSubImage2D| B[textura GL en VRAM]
        C[Timer 12.5Hz] -->|set animation-frame +1| D
    end
    subgraph Slint
        D[Image.animation-frame cambia<br/>SIN invalidar bindings] -->|mark_dirty_region rect| E[partial renderer]
        E -->|blit solo el rect del fuego| F[present]
    end
    B --> D
```

- El paisaje/HUD/seats **nunca se re-renderizan** — su rect no está en la región sucia
- El fuego se actualiza a 12.5Hz como un blit de textura chica
- Los frames se suben a VRAM 1 vez (tiny-skia, 21ms)

## Por qué NO es un "video" (nomenclatura)

El usuario acertó: no es un video, es un **aislamiento de dirty**. La semántica es
"este componente puede cambiar su contenido sin que el árbol global se entere" —
equivalente a un `layer` que no propaga. El nombre correcto sería algo como
`isolated-layer` / `no-propagate` / `animation-source`.

## Qué tocar en Slint (lista concreta)

| Archivo | Cambio |
|---|---|
| `i-slint-core/api.rs` | `Window::mark_dirty_region(rect)` público |
| `i-slint-core/items.rs` | prop `animation-frame` en Image (o item `AnimationImage`) |
| `i-slint-renderer-skia/itemrenderer.rs` | en `visit_image`, si `animation-frame` cambió → `mark_dirty_region(rect)` en vez de invalidar bindings |
| `i-slint-core/item_rendering.rs` | el binding de `animation-frame` se marca `no_tracking` (sin dependencias) |
| `i-slint-compiler/builtins.slint` | declarar la prop nueva |

## Alternativa más simple (sin item nuevo): solo `mark_dirty_region` público

Si exponer `mark_dirty_region` público alcanza, el flujo en Rust sería:

```rust
// Rust: timer de frames
timer.start(80ms, || {
    gl_subtexture(fire_texture, frames[idx]);      // actualiza VRAM
    window.mark_dirty_region(fire_rect);           // repintá solo el fuego
    idx = (idx+1) % frames.len();
});
```

- El `Image` existente muestra `fire_texture` (via BorrowedOpenGLTexture — pero eso
  exige el notifier... ver riesgo abajo)
- **Riesgo:** `BorrowedOpenGLTextureBuilder` requiere el notifier → desactiva partial
  rendering. **Este es el punto que necesita el fork en serio**: el notifier es el que
  mata el partial rendering, y `mark_dirty_region` público SIN notifier es la pieza que
  falta. Hay que verificar si el renderer permite textura prestada sin notifier, o si
  hace falta un item nuevo que gestione la textura internamente.

## Pregunta abierta (a validar en el fork)

¿El renderer puede mostrar una textura GL prestada (`BorrowedOpenGLTexture`) **sin** que
se registre un notifier? Si no, el item `AnimationImage` nuevo (que gestione su propia
textura internamente, sin pasar por la API de notifier) es el camino — y es el fork real.

## Riesgos del fork

1. **Mantenimiento**: re-aplicar el patch en cada upgrade de Slint (iteran rápido)
2. **El renderer es delicado** (buffer age, dirty_region_history[3]): tocar mal =
   artefactos visuales
3. **Texturas prestadas + scale_factor / resize**: hay que re-subir en resize

## Mitigación: PR a GitHub

El mantenedor ya mencionó buffer-age/swap-damage como pendiente para contenido externo.
Un PR bien documentado con:
- `mark_dirty_region` público
- El item `AnimationImage` (o `Image.animation-frame`)
- Benchmarks (este estudio: 9.3 puntos → ~0)

tiene buena chance de ser considerado. Este documento + `docs/performance-study.md` son
el material de apoyo del PR.

## Estimación de esfuerzo

| Pieza | Esfuerzo |
|---|---|
| `mark_dirty_region` público | ~1-2h (API + plumbing) |
| Item `AnimationImage` | ~1-2 días (item + renderer + compiler) |
| Benchmarks + pruebas | ~1 día |
| **Total fork** | **~3-4 días** |
| PR a GitHub (pulido + discusión) | ~1-2 días extra |

## Refinamiento (verificado en el source) — cómo subir la textura sin notifier

**Hallazgo clave que simplifica el fork:**

1. `BorrowedOpenGLTexture` se importa en el renderer vía `import_opengl_texture` (trait
   `Surface`, opengl_surface.rs:179) — **sin requerir el notifier** para dibujarla.
2. Pero la doc exige que la textura se cree en el contexto de Slint, y el único acceso
   público a ese contexto era el notifier → que desactiva partial rendering.
3. **Sin embargo**: `Image::from_rgba8(SharedPixelBuffer)` (que ya usamos en particles.rs
   para los frames RAM) sube la textura al contexto de Slint **sin notifier y sin
   desactivar partial rendering** — funciona hoy, medido.

**La conclusión:** el problema NO es subir la textura (resuelto con `from_rgba8`). El
problema es que **rotar `source` crea una Image NUEVA** (nuevo SharedPixelBuffer →
re-upload + invalida el binding). El fork necesita:

```
Image::from_rgba8 (1 vez, ya funciona) → textura en el contexto de Slint
                                          │
update_frame(pixels)  ← el fork: glTexSubImage2D SOBRE LA MISMA textura
  + mark_dirty_region(rect)              │ (sin crear Image nueva, sin invalidar)
                                          ▼
                        el partial renderer repinta solo ese rect
```

**El item `AnimationImage` propuesto:**
- Crea la textura 1 vez (desde SharedPixelBuffer, como `from_rgba8`)
- `update_frame(&[u8])` → `glTexSubImage2D` en la textura existente + `mark_dirty_region`
- **No** toca bindings → **no** invalida el árbol → el paisaje/HUD nunca se re-renderizan

Esto es el fork mínimo real: un item que expone "mutar mi textura + marcar mi región"
sin pasar por el sistema reactivo de bindings.

---

# ✅ IMPLEMENTADO (2026-08-12) — diseño final y mediciones

## Vía elegida: **A — item `AnimationImage` + registro de frames en el renderer**

Vía B (mutar una `Image` existente) quedó descartada por su barrera de entrada: no hay
API pública para obtener el `ItemRc` de un elemento del árbol desde la app, y exponerla
sería un cambio de API mucho más grande. Vía A le da al item una identidad estable
(`animation-key: int`) que la app direcciona directamente — sin `ItemRc`, sin notifier,
sin tocar el sistema reactivo.

## Arquitectura final

```
Rust (app)                                        Renderer (skia)
─────────────────────────────                     ─────────────────────────────
particles.rs (1 vez):                             SkiaRenderer.animation_registry
  generate_frames() → 60×512×288 RGBA             └─ slots[key]: frames premult. (1 vez)
  window.register_animation_frames(key, frames)      + diff_bboxes[i] (1 vez, ~30ms)
        │                                            + GPU surface (lazy, 1 textura)
  timer 80ms:                                        + last_rect (item en pantalla)
  window.set_animation_frame(key, i) ──► 1. generación++ 
        │                              2. bbox = diff(frame i-1 → i) escalado al rect
        │                              3. mark_dirty_region(bbox) + request_redraw()
        ▼
  render pass: el item se dibuja SOLO si su rect corta el bbox
        └─ draw_animation_image:
              si generación cambió → write_pixels (glTexSubImage2D a la misma textura)
              image_snapshot() → blit escalado al rect del item (sampling nearest/linear)
```

- **GPU path**: el canvas tiene contexto GPU → `canvas.new_surface()` crea UNA superficie
  GL persistente por slot; cada frame es `write_pixels` = un `glTexSubImage2D` a la MISMA
  textura (formato exacto: los frames se premultiplican 1 vez en el registro). Sin imagen
  raster CPU por tick, sin churn del texture cache de Skia.
- **CPU path** (sin contexto GPU — renderer software — o si falla la creación): imagen
  raster reconstruida por cambio de frame (`raster_from_data`). Elegido por slot en el
  primer draw.
- **Repaint mínimo**: `mark_dirty_region` recibe el **bbox del diff de la transición
  old→new** (precomputado en el registro, escalado de coords de frame al rect del item),
  no el rect completo del item. Medido: región sucia 67% → 12-22% de la ventana.
- **Sin bindings**: `set_animation_frame` no toca ninguna `Property`; el tracker de
  rendering del item queda limpio → `compute_dirty_regions` no lo marca → el único dirty
  del frame es el bbox forzado.

## Archivos tocados (fork)

| Archivo | Cambio |
|---|---|
| `vendor/slint-core/items/animation.rs` | **nuevo**: struct `AnimationImage` (width/height/animation-key/image-rendering + cached_rendering_data), impl `Item`, vtable |
| `vendor/slint-core/items.rs` | `mod animation` + `slint_get_AnimationImageVTable` |
| `vendor/slint-core/item_rendering.rs` | `ItemRenderer::draw_animation_image` (default no-op) |
| `vendor/slint-core/partial_renderer.rs` | forward `draw_animation_image` (con tracker de rendering) |
| `vendor/slint-core/renderer.rs` | `RendererSealed::register_animation_frames` / `set_animation_frame` (defaults no-op) |
| `vendor/slint-core/api.rs` | `Window::register_animation_frames` / `set_animation_frame` públicos |
| `vendor/slint-compiler/builtins.slint` | componente `AnimationImage` + `export { AnimationImage as AnimationImage }` |
| `vendor/slint-renderer-skia/animation.rs` | **nuevo**: `AnimationSlot`/`AnimationRegistry` (premultiply, diff bboxes, storage GPU/CPU) |
| `vendor/slint-renderer-skia/lib.rs` | campo registry, impl RendererSealed, `reset_storage()` en set_surface/suspend |
| `vendor/slint-renderer-skia/itemrenderer.rs` | `draw_animation_image` (rect del item, upload lazy, blit) |
| `apps/lumen-slint/ui/voice-view.slint` | `Image` → `AnimationImage { animation-key: 1 }` |
| `apps/lumen-slint/ui/app.slint` | se elimina el plumb de `voice-fire-frame` |
| `apps/lumen-slint/src/particles.rs` | register 1 vez + timer con `set_animation_frame` |

## Decisiones de diseño (y por qué)

1. **El registro vive en el renderer, no en i-slint-core**: el slot necesita tipos de
   skia (Surface/Image) y la textura GL; i-slint-core solo define el trait. La app llega
   vía `Window` (el renderer se obtiene del window adapter).
2. **`reset_storage()` en vez de `clear()` en set_surface/suspend**: el bug crítico del
   primer intento — winit crea la superficie GL *después* del init de la app → el
   `clear()` inicial borraba los frames registrados (slots=0 al primer tick). Los frames
   son datos RAM puros; solo el storage GPU se reinicia y se reconstruye lazy.
3. **Premultiply en el registro**: las superficies GL de Skia son premultiplicadas; la
   conversión se hace 1 vez (60 frames) para que el upload por tick sea formato-exacto
   (sin conversión CPU por frame).
4. **bbox de transición en vez del rect completo**: el contenido animado (fuego+partículas)
   ocupa ~1/4 del item; repintar el bbox real baja la región sucia 67% → 12-22% (~0.7 pts).
   Contrato: el caller avanza de a 1 frame (el modelo "video"); saltos arbitrarios caen
   al rect completo.
5. **`image-rendering` expuesto** para el `pixelated` del pixel-art (nearest sampling);
   el draw es siempre `fill` (estira el frame al rect del item, igual que el `Image`
   original con `image-fit: fill`).
6. **Sin request_redraw en el primer frame**: si `last_rect` aún no existe (el item no se
   dibujó), `set_frame` devuelve None → el primer paint cubre frame 0.

## Mediciones (i5-4590, HD 4600, Mesa, release, main thread, % de 1 núcleo)

Protocolo del estudio: `/proc/<pid>/task/<pid>/stat` (utime+stime, Δ en ventana fija) +
`perf record -F499 -g --call-graph fp` + `perf script --no-inline` filtrado por `^lumen`.
Mismo estado de mic en cada pasada; el audio pesa en threads tokio, no en el main.

| Estado | % de 1 núcleo | Notas |
|---|---|---|
| **Antes** — rotar `source` de Image (estudio 2026-08-11) | 11.5% (rotando) vs 2.2% (congelada) → **+9.3 pts** | Image nueva + alloc + re-upload + invalida bindings |
| **Después** — control (sin animación, item sin slot) | **1.10-1.20%** | mismo voice view, sin rotación |
| **Después** — rotando (item + rect completo) | ~5.06% | primera versión (upload full-frame, dirty = rect del item) |
| **Después** — rotando (item + bbox de transición) | ~4.24-4.36% | dirty 67% → 12-22% |
| **Después** — rotando (+ upload sub-rect) | ~3.87% | upload = sub-rect del diff |
| **Después** — rotando (+ tick cabalgando el push del voice ~10Hz) | **3.37%** | main 0.84% del total; proceso 2.01% del total |
| → **Costo marginal final de la rotación** | **~2.2 pts** | vs control, mismo tamaño de ventana |

**Progresión del costo marginal de rotar (pts de 1 núcleo):**

```
9.3 (rotar source de Image) ──► 4.0 (AnimationImage, rect completo)
     ──► 3.3 (+ bbox de transición) ──► 2.7 (+ upload sub-rect)
     ──► 2.2 (+ el tick cabalga el render del voice push — sin renders propios)
```

**Timing directo por tick (instrumentación temporal, 100 renders por muestra):**
render total ~170µs (compute/traversal ~45µs, drawloop ~85µs, flush ~32µs) y **el draw
del fuego en sí (upload sub-rect ~5µs + blit ~1µs) ≈ 6µs — 0.1 pts**. El costo por render
de un tick de fuego (~1.05ms de CPU medido) está dominado por el present/swap + event
loop, NO por el dibujo del fuego. Por eso el skip de items estáticos no ayudó (el draw de
los estáticos era barato) y el lever final fue ELIMINAR renders (que el fuego cabalgue los
del voice push).

## RAM (RSS medida, /proc)

| Estado | RssAnon |
|---|---|
| Control (sin partículas) | ~37 MB |
| Con partículas (versión final, premultiply in-place) | ~74 MB |
| Mecanismo viejo (rotar Image: Vec<Image> con clones) | ~70 MB extra sobre el control |

- Los frames son 60×512×288×4 = 35 MB (el contenido animado pre-generado; el mecanismo
  viejo guardaba 2 copias ~70 MB — el fork guarda 1 copia, **~33 MB menos**).
- **Premultiply in-place**: los buffers llegan por valor → se premultiplican en su lugar
  (sin copia); el diff bbox se calcula ANTES de premultiplicar (premultiplicar puede
  colisionar y encoger la región de cambio → píxeles viejos).
- La textura GPU es 1 × 512×288×4 = 576 KB (una sola, persistente).

**Experimento documentado — skip de contenido estático vía buffer-age (REVERTIDO):**
se implementó un gate en el partial renderer que salta el re-draw de items estáticos
(tracker limpio) debajo del `AnimationImage` cuando solo cambió la animación y el back
buffer fue reusado (buffer age > 0), apoyándose en que sus píxeles se preservan en el
buffer. **Medido: 3.92% vs 3.87% — ~0 pts de ganancia** (mismo tamaño de ventana). El
costo del repaint no está en los items estáticos (la blit del layer cacheado clipada al
bbox es barata) sino en el draw del propio fuego + clear + submission GL — que es
inevitable (los píxeles cambian). Se revirtió por riesgo de artefactos sin beneficio
medible. El detalle del gate queda documentado aquí para el PR: no perseguir esa vía.

**Resultados clave:**
- **Aislamiento pixel-perfecto**: diff de screenshots separados 120 ms → 0 px fuera del
  rect del item (el paisaje/HUD/seats no se re-renderizan; solo el fuego/partículas).
- **Costo de rotación 3.4× menor**: 9.3 pts → 2.7 pts. La rotación ya no paga: Image
  nueva por tick, alloc 590KB, re-upload full-frame, invalidación de bindings.
- **Optimizaciones aplicadas en orden**: item aislado (sin bindings) → dirty = bbox del
  diff de transición (precomputado en registro) → upload = sub-rect del diff (union
  acumulado si coalescen ticks), premultiply 1 vez en registro.
- **Floor del render por tick** (perf final): el costo restante (~2 pts) es el encoding
  GL del repaint (blit del layer cacheado + blit del fire + comandos), repartido en
  Skia/Mesa sin hot spot dominante; el clear ya está clipado a la región sucia.

## Por qué no llega a 0.3% (y qué falta)

El target "0.3% (como la Image congelada)" asumía que rotar repinta solo el rect chico
sin maquinaria. La realidad medida: **cada tick a 12.5 Hz exige un render pass completo**
(traversal del árbol + GL present) — el partial renderer minimiza el ÁREA repintada, no
la maquinaria. El render vacío (control) cuesta ~0.44 ms; el render del fuego ~2.6 ms.
El floor teórico de la rotación a 12.5 Hz (present+traversal solos) es ~0.5-0.6 pts.

Llegar a 0.3% requiere **swap-damage / buffer-age** (presentar el mismo buffer con solo
la textura actualizada, sin traversal ni present completo) — el trabajo pendiente que el
mantenedor ya mencionó. Este fork es el material: el mecanismo de textura externa que no
invalida el árbol + las mediciones.

## Notas para el PR a GitHub

1. **Problema**: rotar el `source` de una `Image` marca dirty su rect → re-render del
   subárbol → 9.3 pts de un núcleo a 12.5 Hz en HW modesto; reducir el área no ayuda
   (~2.6 pts de ahorro por 6× de área). El costo es el acto de rotar (Image nueva +
   alloc + re-upload + invalidación de bindings), no el tamaño.
2. **Propuesta**: item `AnimationImage` + API `Window::register_animation_frames(key,
   frames)` / `set_animation_frame(key, i)`:
   - El contenido vive fuera del scene graph (registro por renderer, por key).
   - `set_animation_frame` NO toca propiedades → no invalida bindings ni trackers.
   - Repinta solo el bbox de la transición (diff precomputado) vía `mark_dirty_region`.
   - GPU: superficie GL persistente + `glTexSubImage2D` (write_pixels) a la misma
     textura; CPU fallback: raster image. Sin notifier → partial rendering intacto.
   - Contrato: frames del mismo tamaño, avance de a 1.
3. **Benchmarks**: estudio completo en docs/performance-study.md; marginal 9.3 → 2.7 pts
   (item aislado + dirty = bbox de transición + upload sub-rect), aislamiento verificado
   por diff de screenshots (0 px fuera del item).
4. **Scope del fork**: 3 crates vendored (core/renderer-skia/compiler) + 2 archivos de la
   app. Interpreter/C++/femtovg NO tocan (el item es no-op en otros renderers).
5. **Relación con buffer-age**: `mark_dirty_region` público + el registro de texturas
   externas son el paso previo; el PR del mantenedor sobre swap-damage podría consumir
   este mecanismo para llegar al floor real.

