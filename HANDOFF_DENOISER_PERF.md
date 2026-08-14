# Handoff: rendimiento del denoiser en lumen-voice (menos CPU sin perder calidad)

Rol: ingeniero de performance especializado en audio. Repo: `/home/reny/Projects/discord-light`.
Alcance: `crates/lumen-voice/src/audio.rs` (la cadena de supresión), `crates/lumen-voice/build.rs` (build del runtime C), y SOLO si es estrictamente necesario `vendor/faster-enhancer/` (runtime C del denoiser). No toques la UI ni el fork de Slint.

## Contexto

La app de voz tiene un selector manual de 3 niveles de supresión:
- **NS-only** (WebRTC APM: HPF + NS VeryHigh + GC2, sin denoiser externo) — ~0.2% de CPU total.
- **FastEnhancer-S** ("Ligera", hop 512) — ~1.6% de CPU total.
- **FastEnhancer-M** ("Ultra", hop 320) — ~6.4% de CPU total.

Números medidos en el i5-4590 (4C/4T, release), pipeline real APM → FE → limit_peaks:

| Tier | RTF(fe) | % CPU total (4 cores) |
|---|---|---|
| NS-only | 0.0066–0.0095 | 0.2% |
| FE-S | 0.062 | 1.6% |
| FE-M | 0.255 | 6.4% |

El presupuesto del usuario es **≤ 7% de CPU total** (RTF ≤ 0.28). El FE-M ya está justo en el límite; el objetivo de este trabajo es **bajar el CPU del denoiser sin degradar la calidad percibida** (piso de ruido, preservación de voz, pumping).

## Arquitectura actual

- **`NoiseSuppressor`** (`audio.rs` ~908): `processor` (WebRTC APM), `fe: Option<FastEnhancerDenoiser>`. `with_model_and_aec(SuppressorModel, aec)` arma la cadena. `process(frame)` (960 muestras/20 ms): APM en bloques de 480 → tier (FE) → `limit_peaks`.
- **`FastEnhancerDenoiser`** (`audio.rs` ~1259): wrapper del runtime C vendered (faster-enhancer.c). Variantes `new()` (M, frame 320) y `new_small()` (S, frame 512). **Re-framing con buffers**: acumula el input (960) a frames del engine (512/320) y re-emite la salida en bloques de 960 (`in_buf`/`out_buf` Vec<f32>). Convierte i16→f32 por muestra al entrar y f32→i16 al salir.
- **Build dual** (`build.rs`): el runtime C se compila dos veces (Medium sin prefijo + Small con prefijo `fe_s_*`), símbolos prefijados vía `-D`, config forzada con `-include cfg-s/fe_config_medium.h`. Sin profiling (`-DFE_ENABLE_PROFILE=OFF`).
- El runtime C es single-thread, int8 W8A8, kernels AVX2 (8-alineado para S y M — no hay tail escalar en la app).

## Docs de referencia (leerlos primero)

- `docs/performance.md` — tabla de denoisers comparados (GTCRN 0.087/2.2%, DF3 0.233/5.8%, DPDFNet 0.507/12.7%, FE-M...), historial de presupuesto.
- `docs/dev-diary/2026-08-12.md` (sección "16:00 — slice CPU de audio") — diagnóstico por etapa del send path: APM ~140 µs/frame, cadena completa ~254 µs/frame = 1.27% de un core a 50 fps; instrumentación `ns_us`/`enc_us` gateada por `LUMEN_VOICE_DIAG`.
- `docs/dev-diary/2026-08-07.md` — **historial crítico**: un gate por energía (RMS < 0.03, hangover 200 ms) que saltaba la cadena completa en silencio bajó el CPU del send path **−91%** (22.57% → 2.12% de un core), pero **fue revertido** (cortes en bordes de habla — "se escucha cortado"). Cualquier reintento del silence-skip debe resolver ese problema (histéresis + knee suave + hang time, switch solo en gaps de voz).
- `docs/dev-diary/2026-08-09.md` — RTF de los modelos (GTCRN 0.087, DF3 0.233, DPDFNet 0.507 con ORT), el presupuesto 7% = RTF 0.28.

## Áreas de investigación sugeridas (con evidencia del repo)

1. **Overhead del wrapper Rust**: `FastEnhancerDenoiser::process` asigna Vecs por frame (`in_buf.push` por muestra, `drain(..).collect()`, `chunk`/`denoised` nuevos por llamada) + doble conversión i16→f32→i16 en cada etapa. El runtime C es zero-alloc en steady-state; el wrapper no. Medir el costo real (¿cuánto del RTF(fe) 0.062/0.255 es C vs el envoltorio?) y eliminar allocations/conversiones redundantes (buffers pre-asignados, mantener f32 a través de la cadena APM→FE sin pasar por i16).
2. **Pipelining APM‖FE**: la cadena es secuencial (APM luego FE por frame). En 4 cores, un pipeline de 2 etapas (APM del frame N+1 mientras FE procesa el frame N) podría ocultar una de las etapas — con el costo de +1 frame de latencia y complejidad. Evaluar si vale la pena (el APM es ~10× más barato que el FE: 0.0066 vs 0.062/0.255).
3. **Nivel de NS con FE activo**: el APM corre NS VeryHigh ANTES del FE, que ya hace supresión fuerte. ¿Un NS más liviano (High) o NS desactivado cuando el tier FE está activo cambia el resultado audible? (El stack previo GTCRN+NS se des-stackeó por CPU y por "robotización" — ver docs. El FE es post-NS, así que el NS solo alimenta GC2 — medir el impacto real antes de tocar.)
4. **Silence-skip reintentado (el lever grande)**: el historial del 08-07 muestra −91% de CPU saltando la cadena en silencio. El problema fue el corte binario. Un gate bien afinado (histéresis + rampa de ganancia, switch solo en gaps VAD-negativos, usar `speech_detected` que ya existe post-FE) podría recuperar gran parte del ahorro sin el artefacto. El FE-M a 6.4% en llamadas mayormente-escucha es el caso de uso.
5. **Config GC2 / APM**: el GC2 adaptive (headroom 5, max_gain 50, initial 15, max_output_noise −50) corre siempre. Con el FE haciendo la supresión, ¿se puede relajar algo sin cambiar la calidad? Medir antes/después con las métricas.
6. **Runtime C**: si algo del C es mediblemente caro (el perfil por op existe en fe-ab con `FE_ENABLE_PROFILE` — ver `crates/fe-ab/build.rs` para cómo se habilita), evaluar solo cambios de bajo riesgo. El runtime está validado (blob Medium byte-idéntico al vendered; los tiers S/M reproducen las métricas de referencia — ver `crates/lumen-voice/tests/fe_probe.rs`).

## Restricciones duras

- **Calidad**: piso de ruido (floor p10), voz (voice p90) y pumping de los WAVs de referencia NO pueden degradarse fuera de tolerancia (~±2 dB en floor/voice; pumping sin empeorar). WAVs de referencia en `samples/`: `harness_fe_hard36_input.wav` (ruido implantado), `real_speech_es_48k.wav` (grabación con ruido), `mi_voz.wav` (voz limpia).
- **Presupuesto**: objetivo ≤ 7% de CPU total; ideal bajar FE-M por debajo de 5%.
- El selector de 3 niveles y el switch mid-session (rebuild del chain al cambiar de modelo) deben seguir funcionando.
- No romper `crates/lumen-voice` (suite de tests: `cargo test -p lumen-voice --release` debe quedar verde).
- Cualquier cambio al runtime C debe revalidarse con los tiers (fe_probe) y las métricas de referencia.

## Verificación

- Métricas por tier sobre los 3 WAVs: RTF (por etapa APM/FE/chain), floor p10 / voice p90 / pumping (ventanas de 100 ms) — replicar el cálculo de `crates/lumen-voice/tests/fe_probe.rs` (el test `fe_tiers_chain_hard36` tiene las métricas y los umbrales actuales; el test `fastenhancer_tiers_suppress_noise_preserve_tone` tiene el contrato ruido/tóno).
- Antes/después de CADA cambio: tabla comparativa (CPU + métricas). Sin medición no se acepta un cambio.
- El A/B a oído queda para el usuario, pero los números deben estar dentro de tolerancia.
- Benchmark: el test `rtf_bench.rs` de `crates/lumen-voice/tests/` si aplica.

## Uso de subagentes (obligatorio)

Para no ensuciar tu contexto: **máximo UN batch de 3 subagentes**, cada uno con un scope cerrado, y no explores vos mismo inline lo que puedan hacer ellos. Propuesta de reparto:
1. **Scout (investigación)**: perfil del CPU actual por etapa (APM vs FE vs wrapper Rust vs limit_peaks) con el input hard36 — medir allocations del wrapper (o con `perf`/`/proc`) y dar el desglose con números.
2. **Librarian/escritor (propuesta+implementación)**: con el desglose del scout, implementar las 2-3 mejoras de mayor impacto y menor riesgo (buffers pre-asignados / f32-through / silence-skip afinado), midiendo antes/después.
3. **Reviewer**: revisar el diff contra las restricciones de calidad y el historial del repo (especialmente el silence-skip — verificar histéresis/knee/hang time y que no reintroduce los cortes).

Cada subagente entrega: hallazgos + números + archivos tocados. Vos integrás y validás con la suite completa. Si el batch no alcanza, priorizá: primero medir (scout), luego el lever más grande medido.

## Entregables

1. Desglose medido del CPU por etapa (antes).
2. Cambios implementados + tabla antes/después (CPU por tier + floor/voice/pumping por WAV).
3. Suite `cargo test -p lumen-voice --release` verde.
4. Resumen de qué se descartó y por qué (con evidencia).
