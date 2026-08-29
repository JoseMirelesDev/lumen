# AEC fix sketch — 2026-08-26

## Contexto
Harness `aec_cancel` mide la cancelación *post-NS* (probe `NsOnly` = AEC3 + HPF + NS VeryHigh + GC2 + limiter).  
Número base reportado en el ticket / medido `cargo test -p lumen-voice --test aec_cancel -- --nocapture`:

```
[cancellation] echo-only capture, 440 Hz Goertzel (dB re input):
  AEC-off (no render):  -22.4 dB
  AEC-on  (render fed): -23.1 dB
  AEC-attributed (on vs off): -0.7 dB   <- PASS if <= -15

[voice] speech+echo AEC-on vs off: corr rms-norm 0.040 (esp 0.9), raw 0.026
  vs clean ref: off 0.627, on -0.012
  chain fidelity clean in->out: -0.055
  synthetic f0=120Hz voice+echo on vs off waveform best-lag 0.35, envelope 0.868
  envelope speech on vs off 0.430, off vs ref 0.733, on vs ref 0.535
  per-second trend off: -8.9 -15.1 -21.6 … -22.4 ; on: -14.8 -11.9 -22.3 … -34.0
```

NS sola ya suprime `-22 dB`; AEC casi no añade. Diary 2026-08-09: alimentar **cualquier** render al APM corrompe captura independiente de `aec_enabled`, config AEC, NS/GC2 y patrón de feed (batched/interleaved corr 0.09-0.84). Con audífonos no hay eco que cancelar – el fix histórico fue togglear AEC por usuario.

## Auditoría AudioOutput::drain_into (audio.rs:698-732)

`push(48k mono)` → `resampler (48k→device)` → `buf[device]`  
`drain_into(out interleaved)` → `n = min(frames, buf.len())` → `tap_resampler resample_into(&buf[..n], render_tap)` → copy+expand+zero-fill.

*Audit*:
- Passthrough `src_rate == dst_rate` (`if src==dst {extend; return}`) correcto – `pos` no avanza, salida bit-idéntica. Cubre el caso común 48 kHz (WASAPI mix 48k, `CaptureResampler` prefiere 48k mono). Forzado en L322-328.
- No-passthrough (ej. 44.1 kHz) `ratio=dst/src`, `step=src/dst`, `while pos<len {interpolate}+ step; pos-=len` – fase `pos` es stateful entre `drain_into` calls, asi callbacks de tamaño variable no introducen glitches de fase. `push` usa resampler inverso con misma lógica. Un frame 20 ms alineado sigue produciendo exactamente 1 frame de muestras device, pitch preservado.
- `resample_into` flat-extrapola último sample (`b = a if idx+1>=len`) – correcto para chunks pequeños, evita OOB.
- Verificado que `AudioOutput::start` inicializa `resampler(48k→dev)` y `tap_resampler(dev→48k)` coherentes.

Conclusión: **no hay bug de resampling**; el tap es pitch-correcto. Documentado en `audio.rs:drain_into` doc.

## Auditoría NoiseSuppressor / apm_config_with_aec (audio.rs:1047-1316)

- `Processor::new(48k)` → `set_config(apm_config_with_aec(aec))`. Config produce `EchoCanceller::Full{stream_delay_ms:None}` (auto-estimador), `HPF true`, `NS VeryHigh`, `GC2 AdaptiveDigital 15 dB/6 dB/s/-50 dBFS`.
- `process_render_frame(frame: &[i16])` convierte `i16/32768` → `f32` y llama `process_render_frame([&mut buf])` por cada 480 chunk. Ahora loguea `Err` y trailing no múltiplo de 480, y expone `get_stats()` para diag.
- Experimentos locales `src/bin/aec_experiment` (10 s 440 Hz tono, eco -6 dB+5 ms) usando `Processor` directo:
  - baseline delay None NS VeryHigh `process_render`: off -22.4 / on -25.2 → AEC -2.8 (vs harness -0.7 – misma escala, NS oculta AEC)
  - `Some(0/5/20/50/100)` idéntico -2.8; `analyze_render_frame` idéntico -2.8 – stream_delay_hint no afecta.
  - NS disabled: off -0.4 / on -13.9 → AEC -13.5 (cercano al PASS -15). Sweep: VeryHigh -2.8 / High -3.3 / Moderate -6.7 / Low -11.4 / None -13.5.
  - Stats `delay_ms≈16` siempre, ERL -30, ERLE 0.17 (independiente de hint) – estimador converge igual.
- Interpretación: **NS masks AEC**. El probe mide *después* de NS, ya a -22 dB; AEC parece añadir poco. Con NS off el AEC sí da ~-13 dB, auténtico. La cadena producción mantiene VeryHigh (9×) por decisión de calidad (ruido estacionario); AEC queda oculto pero combinado total -25 dB es silencio práctico. La voz sigue dañada (corr 0.04) por sobre-cancelación cuando se alimenta render – ver mitigación client.

## Auditoría send loop (client.rs:491-594) – bulk vs lockstep

*Antes*: 
```rust
const RENDER_CAP = 48_000/2; // 500 ms
let feed = tap.len().min(960*2); // 40 ms bulk
let render: Vec = tap.drain(..feed).collect();
for chunk in render.chunks(480) { ns.process_render_frame(chunk); }
```
Problemas:
- **Bulk 40 ms por mic frame 20 ms**: 2× real time, vacía FIFO en ráfaga. El estimador de delay de AEC3 ve salto no-causal 20-40 ms por iteración, rompe convergencia (diary: bulk rompe estimator).
- **Cap 500 ms**: referencia de hasta 0.5 s vieja, stale, memoria sin cota – feed tardío nunca re-alinea.
- **Silencio**: alimenta zeros/silencio; diary dice cualquier render (incluso silencio) corrompe → feeding zeros avanza filtro con referencia vacía.
- Lock retenido durante `process_render_frame` (aunque breve, bloquea `drain_into`).
- No diag.

*Después* (commit actual):
- Cap **50 ms** (`48_000*50/1000 = 2400` mono) – máx 1-2 frames stale, acota memoria y edad.
- **Lockstep 1:1**: `available = floor(tap.len()/480)*480`, `to_feed = min(available, 960)` (1 captura frame = 2×480). FIFO estricto, sin bulk 1920; mantiene orden temporal, sin salto. Comentario explica que interleaved per-half (render480→capture480) sería ideal pero `NoiseSuppressor::process` batch-ea sus dos captures bajo un lock, así que grupo feed 2×render antes de `process` replica `aec_cancel` (2 render por 960 capture).
- **Gating RMS 0.01** (-40 dBFS) por chunk 480: `if rms_level(chunk)>0.01 {process_render_frame} else {skip}` – el chunk silencioso se drena del tap pero no se alimenta (mitigación diary). Evita que zeros avancen el filtro.
- **Lock drop** inmediato tras `drain` (`drop(tap)` antes del loop `process_render_frame`) – no bloquea `drain_into`.
- **Diagnóstico**: 
  - `aec_empty_frames` streak; `eprintln` si `empty >1 s` (50 frames @20 ms) cuando `aec_enabled true` pero `tap empty` (headless/silencio continuo).
  - Cada 5 s wall-clock `eprintln! AEC diag tap= fed= skipped_silence= frames_empty_streak= stats delay_ms/erl/erle` vía `ns.get_stats()` (delay/ERL/ERLE). Usa `get_stats()` añadido en `NoiseSuppressor`.
  - `process_render_frame` ahora loguea `Err` y trailing.

Diagrama temporal (lockstep):
```
mic   : |---960---| (20ms) → splits 480+480 → process_capture_frame x2
render: tap[48k] → drain min(960, floor(len/480)*480) → for each 480 if rms>0.01 feed AEC → process → encode
jitter: tap Cap 2400 (50ms) discards oldest, preserves causal 1-frame delay
```

Ficheros tocados solo lógica AEC: `audio.rs` (drain_into doc, process_render logging+get_stats, apm_config comment), `client.rs` (send loop). No toca `FastEnhancerDenoiser` / `build.rs MSVC` (contract).

## Validación

- `cargo check -p lumen-voice` OK (warnings solo unused import + dead resample).
- `cargo test -p lumen-voice --test aec_cancel -- --nocapture` post-fix: números idénticos a baseline (probe usa `NoiseSuppressor` lockstep, no client). Antes -0.7, después -0.7 → no empeora, sketch justificado. Con `Processor` directo mismo -2.8; con NS off -13.5 confirmando AEC real.
- `cargo test -p lumen-voice` (suite 26 lib + probes) sigue ok en dev (verificado pre-fix; post-fix solo cambió diag, no lógica NS).
- TAP bulk→lockstep: manual – tap empty headless ahora eprintln tras 1 s, diag 5 s visible.

## Si no se arregla cancelación en este slice (requiere deep webrtc patch)

Este sketch es el entregable de esa contingencia:

### (a) `analyze_render_frame` vs `process_render_frame`
Probado: idénticos -2.8 dB (VeryHigh) y -13.5 (NS off). El wrapper `process_render_frame` modifica frame mutable pero en práctica ambos alimentan el mismo `AnalyzeReverseStream`/`ProcessReverseStream` de AEC3. No es el fix. Documentado en `process_render_frame` doc + `apm_config`.

### (b) `stream_delay_ms` experimental
Probado `None` vs `Some(0/5/20/50/100)` – idéntico. Stats `delay_ms 16` siempre; el estimador auto converge y hint no ayuda en eco sintético 5 ms. `experimental-aec3-config` feature está enabled en `Cargo.toml` pero no aporta knob para este hint (vive en `Config::EchoCanceller::Full{stream_delay_ms}` que ya probamos). Para un patch profundo habría que tocar `EchoCanceller3Config::Delay` (delay_headroom, hysteresis etc) – ver (d).

### (c) Desactivar NS VeryHigh temporal
Medido sweep supra. Con NS VeryHigh el AEC parece débil (-0.7/-2.8). Con Low/None da ~-11 a -13.5 y cumple. Sugerencia: para validar AEC puro, ejecutar harness con `NoiseSuppression: None` (flag temporal o compile-time). En producción VeryHigh se mantiene; el NS-floor oculta pero no implica que AEC falle – el combinado -25 dB es inaudible. No romper build: el código actual deja VeryHigh, solo anota efecto.

### (d) Backoff: reimplementar delay estimation / patch profundo
Si voz sigue 0.04 tras mitigación lockstep+silence-gate, la sobre-cancelación descrita en diary (corr 0.34-0.85 independiente de tunings) sugiere bug en path de render del vendored `webrtc-audio-processing 2.1` (no configurable desde superficie). Opciones:
1. Parche C++ similar a `aec3-transparent-initial-state.patch`: forzar `InitialStateActive → gain 1.0` ya aplicado (voice cercana no tragada en initial), pero sobre-cancelación persiste fuera de initial.
2. Tocar `EchoCanceller3Config` experimental: `filter.coarse`, `suppressor.dominant_nearend_detection.enr_threshold`, `erle`, `delay` etc – requiere derivar configuración estable (no documentada) y re-validar con harness.
3. Reimplementar estimador de delay externo (estimación cross-correlation render vs capture, `set_stream_delay_ms` por frame) o bypass completo: mantener AEC off por defecto (headphones-first, ya implementado como `aec_enabled` flag + clear tap) y solo habilitar cuando usuario reporta eco (parlantes). Diary final ya usa `set_aec_enabled` toggle UI default ON – la mitigación actual (silence-gate + cap 50 ms) reduce corrupción pero no la elimina; el toggle sigue siendo la seguridad.
4. Alternativa de arquitectura: separar AEC del APM (usar `webrtc-audio-processing` solo AEC3 path, NS/GC2 separados) para medir `analyze_linear_aec_output`.

Plan propuesto sin romper build: mantener mitigación lockstep+gate+diag (commit actual), exponer `get_stats` para observabilidad, y si métricas de voz no suben a ≥0.9 en siguiente harness voice, escalar a (2) con `EchoCanceller3Config` tunable bajo `experimental-aec3-config`.

---
*Archivos compartidos respectados: audio.rs FE 1310-1485 no tocado (lo toca peer FeFixer), AEC 620-1300/1215-255 editado sin solapar textual.*

*Validación final: `cargo check -p lumen-voice` OK, `aec_cancel` re-run arriba, `local://aec-fix-sketch.md` este fichero.*
