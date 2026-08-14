# Estudio de rendimiento de la UI — escena de fogata (voice chat)

**Fecha:** 2026-08-11 · **Hardware:** i5-4590 (Haswell), iGPU Intel HD 4600, Mesa 25.2.8 · **Stack:** Rust + Slint 1.17.1 (renderer `renderer-winit-skia`), Skia sobre OpenGL ES 3.0

> Documento consolidado. Los pasos individuales y experimentos están en
> `docs/dev-diary/2026-08-11.md` (9 entradas). Aquí: el panorama, los números
> que importan, lo que se descartó, y el diseño del componente "video" propio
> (fork mínimo de Slint) que resuelve el problema de raíz.

## Resumen ejecutivo

El voice chat tiene una escena de fogata pixel-art (fuego + brasas + luciérnagas +
asientos). El consumo de la UI conectada estaba en ~3.8% de un núcleo, y el usuario
quería bajarlo a nivel idle (~0.5%). El estudio descubrió que **el costo es el modelo
de render de Slint: cada frame de animación que rota un `Image` marca dirty → re-render
del árbol → traversal + allocs + presentación GL**. No hay widget de video nativo en
Slint 1.17 (no decodifica GIF/APNG/WebP), y las APIs para inyectar GL (`set_rendering_notifier`,
`BorrowedOpenGLTextureBuilder`) desactivan el partial rendering → +14% fijo.

La única forma de que el fondo/paisaje no se re-renderice cuando el fuego anima es que
el contenido animado viva **en su propia superficie** (sin invalidar el árbol global) —
un componente primitivo "video" propio. Requiere un fork mínimo de Slint (exponer
`mark_dirty_region` público, o un item que dibuje desde una secuencia de texturas).

## Números clave (todos % de 1 núcleo; ÷4 = % del total)

### Línea base
| Estado | % 1 núcleo | % total |
|---|---|---|
| UI idle (chat normal) | ~0.5% | ~0.13% |
| Voice view mínimo (solo fondo+HUD, nada animado) | ~0.55% | ~0.14% |

### El problema de la animación (FOGATA conectado, mic activo)
| Config | % 1 núcleo | % total | Δ |
|---|---|---|---|
| 34 partículas en el árbol (original) | ~12.25% | ~3.1% | — |
| Partículas → frames RAM, 1 Image grande rotando | ~11.5% | ~2.9% | −0.7 |
| **1 Image grande rotando (12.5Hz)** | **~11.5%** | ~2.9% | — |
| **1 Image grande CONGELADA (frame estático)** | **~2.2%** | ~0.55% | **−9.3** |
| 1 Image CHICA rotando (solo fuego) | ~8.9% | ~2.2% | −2.6 vs grande |

**Conclusión A:** mover 34 items → 1 Image (frames RAM) bajó el render del campfire de
~6.5% a ~0.5% en idle. La consolidación de propiedades (timer único, fuego/seats derivados)
funcionó.

**Conclusión B (la que importa):** el costo de ROTAR (invalidar + re-render + presentar cada
80ms) es ~9.3 puntos, y **reducir el área (Image chica) solo ahorra ~2.6** — el acto de rotar
en sí sigue costando. **No es el tamaño del área, es el acto de rotar.**

### Desglose por hilo (conectado, escena completa)
| Hilo | % de samples | Qué es |
|---|---|---|
| lumen (main) | ~38% | **UI**: render Skia + GL + allocs + timer |
| tokio-rt-worker | ~40% | **Audio**: OPUS encode + WebRTC NS + VAD |
| cpal_alsa_out + pw-data-loop | ~20% | Audio out (ALSA + PipeWire) |

El main conectado (~3.8% de un núcleo) se descompone: presentación GL (swap/dri ~20%),
allocs (~7%), traversal (~6%), bindings (~5%), event loop (~5%), timer de tiles (~8%).

## Lo que ya se optimizó (aplicado y medido)

1. **34 partículas → 1 Image con frames RAM** (`apps/lumen-slint/src/particles.rs`):
   pre-genera 60 frames del ciclo en memoria, rota el `source` de 1 Image. Render del
   campfire: ~6.5% → ~0.5% en idle.
2. **Consolidación de timers**: fuego/asientos/partículas derivan de 1-2 `Property<float>`
   (`time` / `slow-time`) en vez de ~14 timers independientes.
3. **VoiceMeter (espectro real) → indicador discreto**: el level stream a 10Hz ya no
   invalida el árbol (Discord tampoco muestra espectro).
4. **Fondo cacheado** (`cache-rendering-hint`): se rasteriza 1 vez, se blitea.

## Lo que se descartó (con evidencia)

| Vía | Resultado |
|---|---|
| `set_rendering_notifier` + GL overlay | Desactiva partial rendering → **14% fijo** sin importar el árbol |
| `BorrowedOpenGLTextureBuilder` (textura GL) | Exige el notifier → mismo problema, mata partial rendering |
| `request_redraw()` continuo | Fuerza re-render completo → +13% |
| `cache-rendering-hint` + GL overlay | Parpadea (buffer-age mezcla cacheado + GL externo) |
| `cache-rendering-hint` solo (fondo) | Rompe los gradientes radiales en este renderer; no baja el CPU |
| Timer de tiles 50ms → 200ms | wait_deadline baja 60% pero el CPU total NO baja (~0.25% de ahorro) |
| femtovg / wgpu / Vulkan | No cambia el modelo de invalidación; Haswell Vulkan incompleto |
| Image chica rotando | Solo −2.6 puntos; la rotación en sí sigue costando ~6.7 |
| Video/APNG/GIF | Slint 1.17 no decodifica ninguno; no hay widget de video |

## El problema de raíz (por qué no hay atajo en la API pública)

Slint es un **scene graph reactivo genérico**: cuando una propiedad cambia, re-evalúa los
bindings dependientes y re-renderiza la región dirty. No distingue "la brasa se movió por
la animación" de "el usuario arrastró la brasa". El partial rendering limita el ÁREA
repintada pero no elimina el trabajo de CPU por tick (traversal + bindings + comandos GL +
allocs).

No existe API pública para:
- Marcar SOLO una región como sucia sin re-renderizar el resto (`mark_dirty_region` es interno, `RendererSealed`)
- Presentar un frame sin invalidar el árbol
- Reproducir una secuencia de texturas como video

`mark_dirty_region` existe internamente (lo vimos en `i-slint-renderer-skia`), pero es
privado — ese es el hook que el fork necesita exponer.

## Herramientas de medición (instaladas y usadas)

| Herramienta | Estado | Uso |
|---|---|---|
| perf + /proc stat | ✅ | Validación cruzada de CPU |
| inferno (flamegraph) | ✅ instalado | Visual de stacks con pesos — `diag/ui-voice-flame.svg` |
| perf script --no-inline --demangle | ✅ | Desglose por función |
| perf annotate | ❌ no coopera | — |
| tiny-skia (dev) | ✅ bench 21ms/60 frames | Hornear frames con glow/blur |
| Slint MCP | ❌ solo UI, sin perf | — |

## Siguiente paso: componente "video" propio (fork mínimo)

Ver `docs/performance-fork-design.md` para el diseño concreto. En resumen:

- Exponer `mark_dirty_region` público en el renderer, o crear un item que dibuje desde una
  secuencia de texturas sin invalidar el árbol
- El contenido animado (fuego/partículas) vive en su propia superficie; el paisaje/HUD
  nunca se re-renderiza
- Candidato a **PR a GitHub** — el mantenedor ya mencionó buffer-age/swap-damage como
  pendiente para contenido externo

---

# ✅ Slice CPU 2026-08-12: el fork tenía 3 bugs de rendimiento (4.0% → 2.0%)

> Estudio completo: `docs/dev-diary/2026-08-12.md` (08:30). Resumen aquí: qué se
> encontró, qué se cambió, y por qué el piso actual es ~1%.

## Diagnóstico (corrige las conclusiones de arriba)

1. **El atlas vertical de 60 frames (512×17280) no cabe en la HD 4600** (max texture
   16384) → `new_surface` falla → **fallback CPU silencioso: copia de 35 MB por frame**
   (`raster_from_data`), 0.3-0.6 ms por draw. El doc de diseño prometía el path GPU
   (un `glTexSubImage2D` por tick); la implementación con atlas lo rompió en GPUs con
   max texture < 512×60. Fix: superficie de UN frame (512×288) + upload del sub-rect
   del diff por tick.
2. **`TimerBasedFrameThrottle` (X11) produce 2 renders por request_redraw**: su último
   tick del timer encola un RedrawRequested con `pending_redraw` ya falso → cada tick
   del fuego costaba 1 render real + 1 render vacío (-0.00%). Fix en el backend
   vendored: chequear `pending_redraw()` antes de pedir el redraw.
3. **El reloj de 80 ms (`root.time`) quedó obsoleto**: los SceneProp ocultos y el
   ParticleLayer sin uso siguen leyéndolo → cada tick dispara el redraw tracker →
   ~12.5 renders/s vacíos ≈ 2.1% de un núcleo. Fix: eliminado el timer (el
   AnimationImage reemplazó a las partículas del árbol).
4. (menor) **Skip condicional del traversal**: cuando ningún binding cambió desde el
   último render, `compute_dirty_regions` se saltea (el único dirty es `force_dirty`
   del slot del fuego). A/B limpio: 2.73% → 2.13%.

## Números (i5-4590, HD 4600, Mesa, release, main thread, % de 1 núcleo)

| Estado | % main | renders/s |
|---|---|---|
| Fork tal cual (antes de este slice) | **4.0%** | 25 (2 por tick) |
| Atlas fix | 3.8% | 25 |
| + throttle fix | 2.15% | 12.5 |
| + timer 80 ms eliminado | ~2.1% | 12.5 |
| + skip condicional (A/B limpio) | 2.13 vs 2.73 (no-skip) | 12.5 |
| **FINAL (build limpio, conexión real)** | **2.0%** | 12.5 |
| Idle (sin conexión/ticks) | ~0.2% | — |

## El piso real del modelo render-pass

12.5 presents/s × ~0.3-0.5 ms (flush + present) ≈ 0.4-0.6% + idle ≈ **~1%**. Llegar a
0.5-0.9% requiere **swap-damage / buffer-age** (actualizar el sub-rect en el mismo buffer
sin un render pass completo por tick) — el trabajo que el mantenedor ya mencionó. Con el
level stream activo (el usuario hablando) el skip no aplica y el costo sube a ~2.7%.

## Cambios (fork + app)

| Archivo | Cambio |
|---|---|
| `vendor/slint-renderer-skia/animation.rs` | Superficie de un frame + upload sub-rect por tick (fix atlas) |
| `vendor/slint-backend-winit/` (nuevo vendored) + `Cargo.toml` | Fix `TimerBasedFrameThrottle` (1 render por request) |
| `apps/lumen-slint/ui/voice-view.slint` | Timer de 80 ms eliminado (obsoleto) |
| `vendor/slint-core/{renderer,window,partial_renderer}.rs` + `vendor/slint-renderer-skia/lib.rs` | `property_changed()` hook + skip condicional de `compute_dirty_regions` |

---

# ✅ Slice CPU de audio 2026-08-12 (16:00): send chain −60% por frame, AEC off en config

> Detalle completo: `docs/dev-diary/2026-08-12.md` (16:00). Resumen aquí: qué costaba
> el audio, qué se cambió, y los números por hilo. Alcance: `crates/lumen-voice`
> (tokio-rt-worker + cpal + pw-data-loop); la UI quedó intacta.

## De qué estaba hecho el CPU de audio (medido, no asumido)

El tokio-rt-worker conectado (mic silencioso, ns-only, aec=false, 0 peers) era ~3.1% de
un núcleo, compuesto por:

| Componente | % de 1 núcleo | Notas |
|---|---|---|
| **encode opus del silencio** | ~1.0 | el mic real entrega ruido de piso (-57 dBFS), NO silencio digital → el VAD de opus no activa DTX → codifica full-rate (154-161 B/frame, ~112-170 µs) |
| **cadena NS (webrtc APM)** | ~0.7 | HPF + NS VeryHigh (RNN-VAD + three-band + SincResampler 48→16k) + GC2 |
| churn de reconnects | ~0.4-1.0 | el backend cae cada ~2-60 s en esta máquina; cada reconnect reabre streams pipewire (visible en el perfil como `pw_stream_new`) |
| misc (webrtc/tokio) | ~0.3 | mio, DTLS/ICE, playout 0 peers (5 µs/tick) |

Dato clave del bench por-etapas (`tests/send_chain_cpu.rs`): con silencio DIGITAL el
encode cuesta 31 µs (DTX, 1 B); con el piso de mic real 112-170 µs. `set_complexity`
6/4 EMPEORA el encode (VAD misclasifica). El `FilterCore`/`xcorr_kernel` del perfil
anterior era AEC3 con render alimentado (sesión con aec stale=true); con aec=false y
sin render, AEC3 cuesta ~5 µs/frame.

## Cambios (lumen-voice)

1. **Skip del encode en silencio (H1)** — `silence_reuse_decision()` en `audio.rs`,
   usada por el send loop (`client.rs`): silencio confirmado (post-NS `speech_detected`
   o rms crudo > 0.01) → los 2 primeros frames se codifican y cachean, el resto
   reutiliza el último paquete de silencio (RTP avanza seq/ts; el remoto decodifica el
   mismo silencio); refresh cada ~8 s. La voz NUNCA se cachea. Test del contrato
   (`tests/silence_path.rs`): 116/116 frames de voz nunca cacheados, energía de voz
   preservada 97%.
2. **AEC off en el config del APM (H2)** — `apm_config_with_aec()`: con
   `aec_enabled=false` el config no crea AEC3 (sin matched filter/xcorr). El APM se
   reconstruye si cambia el modelo o el flag.
3. Herramientas: `p2p_diag --model/--aec-off`, hook `LUMEN_AUTOJOIN` (app),
   instrumentación diag por-etapa (`ns_us`/`enc_us`/`tick_us`/`pkt_len`/`reused`).

## Números (i5-4590, release, app real, mic silencioso, 0 peers, % de 1 núcleo)

| Métrica | ANTES | DESPUÉS |
|---|---|---|
| encode por frame (p50) | 388 µs | 1-3 µs (98% reuso) |
| frame de envío total (p50) | ~590 µs | ~180-250 µs |
| send chain a 50 fps | ~2.95% | ~0.9-1.25% |
| tokio-rt-worker (perf) | 3.07% | steady-state ~1-1.3% (NS 0.7 + misc); 2.3-3.9% según churn |
| pw-data-loop + cpal_alsa_out | ~1.6-2.1% | igual (fuera del alcance del cliente: quantum de PipeWire) |

Piso restante del audio: la cadena NS (~0.7%, RNN-VAD incluido) + el churn de
reconnects del backend (~0.4-1%) + pw-data-loop (~1%, ciclo del grafo de PipeWire).
Reducir la NS tocaría calidad (fuera de alcance); el quantum de PipeWire no se puede
cambiar desde el cliente (cpal `BufferSize::Fixed` no lo propaga — medido).

---

# ✅ Slice reconnect in-place 2026-08-12 (18:30): churn de audio eliminado + quantum A/B

> Detalle completo: `docs/dev-diary/2026-08-12.md` (18:30). Resumen aquí: la conexión
> WS al DO caía cada ~2-60 s; cada caída mataba la sesión completa (streams de audio
> reabiertos, underruns, silencio audible ~10 s). El churn era en gran parte
> AUTO-INDUCIDO: cada re-join de la app creaba un WS nuevo que el DO evictaba por
> dedup de usuario (close 4000) → "caída" → re-join… Con una conexión estable: **0
> caídas en 15+ min**.

## Cambio (lumen-voice)

| Archivo | Cambio |
|---|---|
| `crates/lumen-voice/src/client.rs` | **Reconnect in-place**: `SignalEvent::Closed{replaced:false}` ya NO hace teardown — `run_loop` reconecta el WS con las mismas credenciales (backoff 2 s→30 s) mientras la sesión (mic, output, send/playout/levels) sigue viva; peers stale se cierran y el re-join los recrea (Joined/PeerJoined/Offer); `Joined` re-sincroniza el peer set (PeerLeft para los que se fueron). `Replaced` sigue con teardown completo. El host nunca ve `Signaling::Closed` en un drop simple → no hay segunda sesión → streams intactos. + test unitario con WS local (`ws_drop_reconnects_in_place_without_teardown`) |
| `crates/lumen-voice/src/signaling.rs` | log de close 4000 (replaced) para distinguir evicción de drop |

## Medición (app real, voice abierto, mic silencioso, 0 peers)

| Métrica | ANTES | DESPUÉS |
|---|---|---|
| Caídas WS | 1 por ~30-90 s (auto-inducidas) | 0 en 15+ min |
| Streams (cpal_alsa_in/out) | TIDs nuevos por ciclo | TIDs estables |
| send_exit / teardown | 1+ por ciclo | 0 |
| Gap de playout por ciclo | **10.842 ms** | 0 |
| Underruns en el ciclo | de arranque | 0 |
| tokio steady-state | ~1-1.3% (perf) | **1.60%** (thread_cpu, sin diag; el diag suma ~0.8-1.0%) |

## Quantum de PipeWire 1024→2048 (A/B medido, REVERTIDO)

`pw-metadata -n settings 0 clock.force-quantum 2048` SÍ mueve el grafo (verificado por
wakeups de pw-data-loop: 23.5/s = 48000/2048; el metadata `clock.quantum` queda stale).
Ganancia real: pw-data-loop 1.03%→0.38%, cpal_alsa_out 0.70%→0.35% (−1.0 pt combinado).
**Pero con artefactos**: XRUNs continuos (1000+ líneas de "buffer underrun or overrun"
en ~2 min, voice + sfx), ring de salida saturado (p50 3 frames vs 0-1) y shed ~24.000
samples/s (≈50% del audio). Causa: los ALSA clients de la app negocian buffers chicos
(playback 512) y a quantum 2048 el mismatch underrunea; la app no puede pedir buffer
mayor (cpal no propaga). **Revertido** (`force-quantum 0`): estado ~baseline (0.90% +
0.67%, ring 0, shed 0, 1 XRUN de arranque). Un cambio global limpio exigiría
`default.clock.quantum=2048` en la config + restart de pipewire y de TODOS los clientes,
con +21-42 ms de latencia en todas las apps — no se hizo (disruptivo).

---

## RAM: frames dispersos con paleta (2026-08-12, 16:00)

Los frames del campfire son pixel art: ~2135 píxeles visibles de 147456 por frame
(98.5% transparente) y 11-22 colores RGB únicos. `register` los convierte a una paleta
compartida de coincidencia exacta (179 colores — sin pérdida) + lista dispersa de
píxeles `(x, y, idx, alpha)` de 6 B. **35.4 MB → 0.77 MB**, RssAnon 69.7 → 33.5 MB
(−52%) con `malloc_trim(0)` tras el registro. El decode por tick solo reconstruye el
sub-rect del diff (~1500 px) en un scratch reutilizado; CPU sin cambio (2% main).

---

# Línea de tiempo consolidada (antes de TODO → ahora), % de 1 núcleo

Unifica los dos slices (UI 08:30 + audio 16:00) en la misma unidad. Las celdas
marcadas con `†` se estiman de las proporciones del desglose original del estudio
(main ~38% / tokio ~40% / cpal+pw ~20% de samples, con el main en 4.0% de 1 núcleo);
el resto son mediciones perf -F 99 de ventanas de 15-30 s, mismas condiciones (voice
abierto, mic silencioso, 0 peers).

| Hito | main (UI) | tokio-rt-worker | pw-data-loop | cpal_alsa_out | proceso total |
|---|---|---|---|---|---|
| **Antes de TODO** (pre-UI, fork 4.0%) | ~4.0% | ~4.2% † | ~1.5% † | ~0.6% † | **~10.5% †** |
| Post-UI, pre-audio (build 8426, AEC stale=true) | 2.09% | 6.4% | 1.41% | 1.35% | 11.4% |
| Base del slice de audio (app5, aec=false) | 0.24-0.37% | 3.07% | 1.13% | 0.73% | 5.2% |
| Slice audio (mediana de 4 ventanas de 30 s) | 0.27-0.37% | 2.0-2.3% | 0.74-1.04% | 0.64-1.01% | 4.0-4.5% |
| **Ahora** (reconnect in-place; sin diag, thread_cpu) | 0.2-0.3% | **~1.6%** | ~0.9% | ~0.7% | **~3.5-4.0%** |

Notas:
- El pico de tokio de 8426 (6.4%) es el estado con AEC3 activo (settings stale=true →
  matched filter/xcorr corriendo con render alimentado); el slice de audio lo eliminó
  en dos pasos: settings correctos (−~1.7%) + skip de encode en silencio (−~1.0%).
- El slice reconnect in-place (18:30) eliminó el churn: 0 caídas en 15+ min (antes
  1 por ~30-90 s), 0 send_exit/teardown, streams nunca reabiertos, gap de playout de
  10.8 s por ciclo → 0. El tokio post-fix es ~1.6% (thread_cpu, sin diag; el diag suma
  ~0.8-1.0%). El quantum de PipeWire a 2048 baja pw-data-loop+cpal_alsa_out −1.0 pt
  pero produce XRUNs continuos (A/B medido y revertido).
- La UI queda en el piso del modelo render-pass (~12.5 presents/s ≈ 1%); el audio queda
  en la cadena NS (~0.7%) + pw-data-loop (~1%, ciclo del grafo de PipeWire, fuera del
  alcance del cliente; 2048 medido y revertido por artefactos).
