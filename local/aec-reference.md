# AEC referencia — investigación para lumen-voice (2026-08-26)

> Contexto: `webrtc-audio-processing 2.1` (bundled) vía `tonarino/webrtc-audio-processing` + `cpal` @48 kHz mono. Harness `aec_cancel`: AEC-attributed -0.7 dB (PASS -15 dB), voz corr 0.04. NS VeryHigh sola ya da -22 dB; con NS=None AEC da -13.5 dB. Cap 50→150 ms, gate 0.01→0.002, `analyze` vs `process` sin mejora. Larsen en `mic_test` persiste. Necesita referencia externa probada — no tuning a ciegas.

---

## TL;DR — recomendación para próximo build

**Opción A (recomendada, 2-3 días): quedarnos en `webrtc-audio-processing` 2.1 pero romper el monolit APM y tunear AEC3 vía `EchoCanceller3Config` experimental.** Separar AEC3 del NS/GC2 del APM: `webrtc-audio-processing` solo AEC3 path (`filter.export_linear_aec_output=true` + `NoiseSuppression.analyze_linear_aec_output=true`), y medir ERLE con `get_stats()`. Tunear `suppressor.{normal,nearend}_tuning`, `delay.delay_headroom_samples`, `filter.{refined,coarse}.length_blocks` y `erle`. Mantener el lockstep 1:1 + silence-gate del sketch, añadir medición objetiva con harness provisional `NS=None`.

**Opción B (3-5 días, plan B si A no lleva voz corr ≥0.9): migrar AEC a `sonora` (pure-Rust AEC3 port M145, 2400+ tests C++ pasando) o `sonora-aec3` solo, y dejar `webrtc-audio-processing` solo para HPF/NS o reemplazarlo por `sonora-ns`/`DeepFilterNet`. Elimina FFI C++ del send path, habilita debug de filtro lineal y `analyze_linear_aec_output` sin feature experimental.**

*SpeexDSP / `aec-rs` descartado como reemplazo principal*: calidad inferior a AEC3, sin drift compensation ni DTD moderna, mantenimiento 2022, solo justificable en embedded/WASM legacy.

---

## 1) webrtc-audio-processing 2.1 — uso correcto de AEC3

### 1.1 Qué es cada versión

| Upstream | Rust crate | Estado |
|---|---|---|
| freedesktop `webrtc-audio-processing` 1.3 (autotools) | `webrtc-audio-processing` 0.5.0 | old, docs.rs OK, API ~idéntica pero sin `experimental-aec3-config` madura |
| freedesktop `webrtc-audio-processing` 2.1 = WebRTC M115+ (meson) | `webrtc-audio-processing` 2.1.0 (2026-05-13, `tonarino` 323★) | **actual** — la que usa lumen-voice (`~2.1`). Major trackea upstream, no semver estricto. `2.1.0` falló build en docs.rs, usar `0.5.0` para leer docs y repo para el resto |

- Homepage: https://www.freedesktop.org/software/pulseaudio/webrtc-audio-processing/
- Git: https://gitlab.freedesktop.org/pulseaudio/webrtc-audio-processing
- Rust wrapper: https://github.com/tonarino/webrtc-audio-processing
- docs.rs (wrapper 0.5 succeeder legible): https://docs.rs/crate/webrtc-audio-processing/0.5.0
- Crate: https://crates.io/crates/webrtc-audio-processing (78k downloads sys, activa 2025-2026)

### 1.2 Pipeline real y el error común

Orden canónico confirmado en `lib.rs` + `switchboard.audio` + `forasoft.com` + `gaudiolab.com`:

```
HPF → AEC3 (linear adaptive filter + residual suppressor) → NS → AGC2
```

`NS` **siempre después** de AEC3. Si NS va antes, su no-linealidad (spectral subtraction) rompe la convergencia del filtro adaptativo. Tema del hallazgo local: NS VeryHigh enmascara AEC (-22 dB NS sola ⇒ AEC -0.7 dB medido post-NS vs -13.5 dB con NS=None). No es que AEC falle 100%, es que el probe mide post-NS.

`analyze_linear_aec_output` (flag en `config::NoiseSuppression`) expone la salida del filtro lineal **antes** del suppressor no-lineal, para que NS no lo tape. Requiere C++ `filter.export_linear_aec_output=true` — solo accesible con `experimental-aec3-config`. Sin eso, `cargo test` en `lib.rs: test_full_aec_with_linear_aec_output_misconfiguration` pasa pero el flag se ignora silenciosamente (ver `lib.rs:set_config`).

### 1.3 `stream_delay_ms` — no es un knob de tuning, es un hint

- `Config::EchoCanceller::Full { stream_delay_ms: Option<u16> }` vs `Mobile { stream_delay_ms: u16 }`. `None` = auto-estimador AEC3 (delay estimator + clock-drift detector). `Some(ms)` fuerza `set_stream_delay_ms()` **antes de cada `process_capture_frame`** (ver `lib.rs: Processor::process_capture_frame`).
- Experimento local (sketch): `None` vs `Some(0/5/20/50/100)` idéntico -2.8 dB (VeryHigh) y -13.5 dB (NS off); stats `delay_ms≈16` siempre, ERLE 0.17. El estimador converge igual en eco sintético 5 ms — por eso el hint no ayudó.
- Conclusión: solo fijar `stream_delay_ms` si tienes **medición externa** (cross-correlation render↔capture o timestamp HAL). Para cpal/WASAPI sin HAL timestamp, dejar `None` y arreglar framing/jitter es más rentable que forzar delay.

### 1.4 `process_render_frame` vs `analyze_render_frame`

```rust
// lib.rs — ambos alimentan el mismo ReverseStream de AEC3
processor.process_render_frame([&mut buf])  // puede modificar buf (AGC del reverse)
processor.analyze_render_frame([&buf])     // solo analiza, no modifica
```

Probado local: idénticos -2.8 / -13.5. `process_render` modifica frame mutable pero en la práctica ambos llaman `AnalyzeReverseStream`/`ProcessReverseStream` de AEC3. **No es el fix.** Usar `analyze_render_frame` cuando no necesitas modificar reverse (más barato, semánticamente correcto) — el wrapper actual `NoiseSuppressor::process_render_frame` usa `process_render_frame`; cambiar a `analyze` es cosmético pero recomendado.

### 1.5 Framing 10 ms — la causa #1 de AEC roto

`Processor::num_samples_per_frame() = sample_rate / 100` → **480 @48 kHz, 160 @16 kHz**. Cada `process_*_frame` **debe** recibir exactamente N. El harness local ya lo hace (chunks_exact 480). El bug histórico `RENDER_CAP 500ms + bulk 40ms por mic 20ms` rompía el estimador (salto no-causal 20-40 ms). El sketch lo arregla a `cap 50ms + lockstep 1:1 (to_feed = min(available,960) floor 480) + silence-gate RMS 0.01`. Copiar ese patrón es obligatorio para cualquier opción.

Snippet mínimo copiar-pegar (del `simple.rs` oficial + sketch):

```rust
use webrtc_audio_processing::{Processor, Config, EchoCanceller};
use webrtc_audio_processing::config::{HighPassFilter, NoiseSuppression, NoiseSuppressionLevel, GainController2, AdaptiveDigital, FixedDigital};

let ap = Processor::new(48_000).unwrap();
ap.set_config(Config {
    echo_canceller: Some(EchoCanceller::Full { stream_delay_ms: None }), // auto
    high_pass_filter: Some(HighPassFilter { apply_in_full_band: true }),
    noise_suppression: Some(NoiseSuppression { level: NoiseSuppressionLevel::VeryHigh, analyze_linear_aec_output: false }),
    gain_controller: Some(GainController2 { .. }), // ver audio.rs:1227
    ..Default::default()
});

// Render thread (cpal output callback → tap ring):
let mut tmp = [0f32; 480];
for chunk in far.chunks_exact(480) {            // far: &[i16] @48k mono
    for (i,s) in chunk.iter().enumerate() { tmp[i] = *s as f32 / 32768.0; }
    ap.analyze_render_frame([&tmp]).unwrap();   // o process_render_frame si necesitas mod
}
// Capture thread (cpal input callback, 20 ms = 960 = 2×480):
for (in_c, out_c) in capture.chunks_exact(480).zip(out.chunks_exact_mut(480)) {
    for (i,s) in in_c.iter().enumerate() { tmp[i] = *s as f32 / 32768.0; }
    ap.process_capture_frame([&mut tmp]).unwrap();
    for (i,v) in tmp.iter().enumerate() { out_c[i] = (v*32767.0).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16; }
}
let stats = ap.get_stats(); // delay_ms, echo_return_loss, echo_return_loss_enhancement
```

Links copia:
- https://github.com/tonarino/webrtc-audio-processing/blob/master/examples/simple.rs
- https://github.com/tonarino/webrtc-audio-processing/blob/master/src/lib.rs
- https://github.com/tonarino/webrtc-audio-processing/blob/master/src/config.rs

### 1.6 `experimental-aec3-config` — tuning fino real

Feature: `webrtc-audio-processing = { version="~2.1", features=["experimental-aec3-config"] }` → activa `bundled` (necesita clang/meson/ninja, headers privados). Sin semver.

API (`src/experimental.rs`):

```rust
use webrtc_audio_processing::experimental::EchoCanceller3Config;
let mut cfg = EchoCanceller3Config::default(); // single-channel
// o ::multichannel_default() si pipeline.multi_channel_* = true
cfg.suppressor.normal_tuning.mask_lf.enr_suppress = 0.4; // default
cfg.suppressor.normal_tuning.mask_hf.enr_suppress = 0.10;
cfg.suppressor.dominant_nearend_detection.enr_threshold = 0.25;
cfg.suppressor.dominant_nearend_detection.snr_threshold = 30.0;
cfg.delay.delay_headroom_samples = 32;
cfg.filter.refined.length_blocks = 13; // 13×4ms = 52ms tail
cfg.erle.min = 1.0; cfg.erle.max_l = 4.0;
assert!(cfg.validate()); // ¡obligatorio! clamp a rangos y clamp entre min/max
let ap = Processor::with_aec3_config(48_000, cfg).unwrap();
// Nota lib.rs: with_aec3_config fuerza echo_canceller=Full y desactiva selección
// automática multichannel — usar multichannel_default() si es estéreo.
```

Defaults completos (copiables) en: https://github.com/tonarino/webrtc-audio-processing/blob/master/examples/aec-configs/defaults.json5 (y `multichannel-defaults.json5`). Deriva de WebRTC `echo_canceller3_config.h`. Campos clave para lumen:

| Grupo | Campo | Default | Qué hace |
|---|---|---|---|
| `suppressor.normal_tuning` | `mask_lf.enr_suppress` 0.4, `mask_hf.enr_suppress` 0.1 | ganancia de supresión residual en low/high. Subir→más eco cancelado pero voz más hueca/puerta. El test `test_aec3_configuration_tuning` en `lib.rs` demuestra 3 dB diferencia al mover 0.1→5.0 |
| `suppressor.dominant_nearend_detection` | `enr_threshold 0.25`, `snr_threshold 30`, `hold 50` | DTD — detecta double-talk (voz local dominante) y baja supresión. Si voz corr 0.04, probar `enr_threshold 0.4` y `snr_threshold 20` para menos agresivo |
| `delay` | `delay_headroom_samples 32`, `hysteresis 1`, `num_filters 5` | margen para jitter de delay. 32 @48k = 0.66 ms. Si capture/render tienen drift, subir a 64-128 |
| `filter.refined` | `length_blocks 13`, `leakage 5e-5`, `noise_gate 2e7` | longitud de cola de eco (13×4ms=52ms). Para sala reverberante/Larsen subir a 20 (≈80ms) + costo CPU |
| `erle` | `min 1, max_l 4, max_h 1.5` | Echo Return Loss Enhancement — si ERLE 0.17 constante, probar `max_l 8` |
| `buffering` | `max_allowed_excess_render_blocks 8` | cuántos bloques render pueden exceder a capture antes de descartar. 8 = 80ms. Relacionado con cap del tap |

Link: https://github.com/tonarino/webrtc-audio-processing/blob/master/src/experimental.rs

---

## 2) SpeexDSP AEC — alternativa ligera

### Qué es

- C lib `speexdsp` (Xiph, MDF adaptive filter) — bindings Rust `speexdsp 0.1.2` (2022-04-28, 0% docs, `rust-av/speexdsp-rs` 26★). Feature `sys` usa C; sin feature usa reimplementación Rust pura incompleta.
- Wrapper ergonómico `aec-rs` (aka `aec`) https://github.com/thewh1teagle/aec — `aec-rs 1.0.0` (2024-12), ~9.6k downloads, incluye `aec-rs-sys` con speexdsp precompilado + C header, Python `pyaec`. Soporta Win/Linux/macOS/Android/iOS/WASM/RISC-V.

### Pros

- Sin C++ toolchain pesado (solo clang/pkg-config si `speexdsp-sys` sys), binario pequeño.
- API simple: `Aec::new(frame_size, filter_length, sample_rate)` + `cancel(capture, reference)` por frame. Fácil con cpal 48k mono.
- VAD + AEC + resampler incluidos en speexdsp (aunque VAD básico).
- Precompilado en `aec-rs` — no necesita meson/ninja/abseil.

### Contras — por qué no usarlo como reemplazo principal

- **Calidad vs AEC3**: speexdsp = MDF lineal clásico, sin supresor residual no-lineal ni robustez a distorsión no-lineal de altavoz (nonlinear tail). WebRTC AEC3 usa multi-filtro PBFDAF + RES + drift compensation + DTD moderna. En comparación directa (switchboard.audio, forasoft, Meta atscaleconference): AEC3 gana en double-talk (evita "walkie-talkie") y Larsen.
- **Sensibilidad a drift/jitter**: speexdsp no compensa clock drift capture↔render — con cpal WASAPI 48k puede divergir lento; AEC3 sí.
- **Mantenimiento**: `speexdsp` upstream estancado (feature-complete, solo bugfixes), Rust `speexdsp` 0.1.2 sin update desde 2022, `aec-rs` estable pero sin iteración AEC3-level.
- **Sin GC2/NS moderno**: si reemplazas todo APM por speexdsp, pierdes NS VeryHigh y AGC2; tendrías que añadir NS separado (RNNoise/DeepFilterNet) y recrear pipeline.

### Snippet integración Rust cpal 48k mono (copiable)

```rust
// Cargo.toml
// aec-rs = "1.0"
// cpal = "0.15"
use aec_rs::Aec; // wrapper speexdsp
use parking_lot::Mutex;
use std::sync::Arc;
use ringbuf::{HeapRb, traits::*}; // o tu ring

const FRAME: usize = 480; // 10 ms @48k — speexdsp también pide frame fijo
// speexdsp filter_length ≈ tail_ms * rate / frame; 80ms tail → ~8*480
let mut aec = Aec::new(FRAME, 2048, 48_000).unwrap();

// Tap render: en cpal output callback copias lo que va al speaker
// (ya en 48k mono float) a un ring; en capture callback:
let mut tmp_ref = [0f32; FRAME];
let mut tmp_cap = [0f32; FRAME];
// drain 1:1, skip silencio (mismo gate que sketch):
for (cap_chunk, out_chunk) in capture.chunks_exact(FRAME).zip(output.chunks_exact_mut(FRAME)) {
    // referencia alineada temporalmente (ring delay ~5-30ms real)
    if let Some(ref_chunk) = render_ring.pop_chunk(FRAME) {
        if rms(&ref_chunk) > 0.002 { // gate 0.002 probado sin mejora, pero evita zeros
            tmp_ref.copy_from_slice(&ref_chunk);
        } else { tmp_ref.fill(0.0); }
    } else { tmp_ref.fill(0.0); }
    tmp_cap.copy_from_slice(cap_chunk); // cap f32 -1..1
    // speexdsp AEC — in-place
    aec.process(&mut tmp_cap, &tmp_ref); // firma real: cancel/process según versión
    out_chunk.copy_from_slice(&tmp_cap);
}
```

> Nota: firma exacta `aec-rs` es `Aec::process` / `cancel` según versión — ver `examples/usage.rs` https://github.com/thewh1teagle/aec/blob/main/examples/usage.rs. Verificar con `cargo doc --open -p aec-rs`. Frame y filter_length deben coincidir entre ctor y `process`.
> Alternativa directa speexdsp sin `aec-rs`: https://github.com/rust-av/speexdsp-rs (requiere `clang` + `libspeexdsp-dev`, feature `sys`).

Links:
- https://docs.rs/crate/speexdsp/0.1.2
- https://github.com/rust-av/speexdsp-rs
- https://github.com/thewh1teagle/aec
- https://crates.io/crates/aec-rs
- https://www.speex.org/docs/manual/speex-manual/node7.html (API C original)

---

## 3) crates.io — landscape AEC Rust

| Crate | Versión / fecha | Downloads | Mantenimiento | Tipo | Veredicto lumen |
|---|---|---|---|---|---|
| `webrtc-audio-processing` 2.1.0 | 2026-05-13 | 78k sys | **activo** (tonarino, releases Q1-Q2 2026) | Wrapper C++ PulseAudio repack | ✅ actual, seguir aquí |
| `sonora` (+ `sonora-aec3`, `sonora-agc2`, `sonora-ns`) | 2026-07 (M145 port, Rust 1.91) | ~1k (nuevo) | **activo** (dignifiedquire, BSD-3, CI + val 2400 tests C++ vs Rust) | Pure Rust AEC3/NS/AGC2, SIMD SSE2/AVX2/NEON | ✅ plan B recomendado |
| `sonora-aec3` solo | id | - | activo | Pure Rust AEC3 | ✅ si solo necesitas AEC |
| `aec3` (RubyBit/aec3-rs) | 0.2 graph/DAG | bajo | activo mid-2026 | Pure Rust AEC3 graph | ✅ alternativo a sonora si quieres DAG |
| `aec-rs` (thewh1teagle/aec) | 1.0.0 2024-12 | ~9.6k | estable/quiet | Wrapper speexdsp | ⚠️ solo embebido/WASM |
| `speexdsp` (rust-av) | 0.1.2 2022-04 | bajo | estancado (0% docs) | Bindings speexdsp | ⚠️ indirecto vía aec-rs |
| `fdaf-aec` | 2025-06 | ~400 | experimental/quiet | Pure Rust FDAF Overlap-Save | ⚠️ lightweight, sin AEC3 quality |
| `webrtc-aec`, `echo_cancellation`, `aec` genéricos | no existen / no mantenidos | - | - | - | ❌ no usar |

Búsqueda: `cargo search aec` hoy devuelve `sonora`, `sonora-aec3`, `aec3`, `aec-rs`, `fdaf-aec`, `webrtc-audio-processing`, `speexdsp` — no hay `webrtc-aec` ni `echo_cancellation` separados útiles.

Links:
- https://crates.io/crates/webrtc-audio-processing
- https://crates.io/crates/sonora / https://github.com/dignifiedquire/sonora
- https://crates.io/crates/sonora-aec3
- https://crates.io/crates/aec3 / https://github.com/RubyBit/aec3-rs
- https://crates.io/crates/aec-rs
- https://crates.io/crates/fdaf-aec / https://www.reddit.com/r/rust/comments/1lkx73m/super_lightweight_fast_aec_in_rust_built_a_native/
- https://crates.io/crates/speexdsp

---

## 4) Discord / Krisp — referencia de pipeline AEC+NS

### Qué hace Discord (verificado)

- WebRTC APM base (Discord blog: https://discord.com/blog/how-discord-handles-two-and-half-million-concurrent-voice-users-using-webrtc)
- Krisp SDK **deep-learning NS** integrado como reemplazo del NS de WebRTC (no RNNoise nativo). Discord: `HPF → AEC → Krisp NS → AGC`. Krisp corre on-device, solo NS, no AEC. RNNoise solo existe vía wrapper externo (OBS/virtual cable) — no es nativo.
- Double-Talk Detector (DTD) es subcomponente de AEC, no módulo aparte. Si AEC confunde voz local con eco durante double-talk, corta voz ("robotic", "walkie-talkie"). Discord sufre lo mismo si AEC mal configurado.

Links:
- https://discord.com/blog/how-discord-handles-two-and-half-million-concurrent-voice-users-using-webrtc
- https://support.discord.com/hc/en-us/articles/360040843952-Krisp-FAQ
- https://krisp.ai/discord/

### Orden AEC vs NS — por qué importa

Papers / guías coinciden (ISCA Kang15, vocal.com, Biamp, Harman):

- **AEC primero, NS después**. AEC es lineal adaptativo; NS introduce no-linealidades (spectral gating) que destruyen convergencia si va antes.
- Si NS va antes, el filtro adaptativo ve señal distorsionada y diverge; si AEC va primero, NS limpia residuo + ruido ambiente sin confundirse con eco correlacionado de alta energía.
- Soluciones avanzadas: estimación conjunta (joint AEC+NS) — paper Kang15 https://www.isca-archive.org/interspeech_2015/kang15_interspeech.pdf, Schwartz24. No necesario en lumen si mantenemos orden canónico.
- **Discord/Krisp siguen AEC→Krisp** por esa razón. Lumen hoy hace lo mismo (`AEC3 → VeryHigh NS`), pero mide post-NS (por eso -0.7). Krisp no tapa AEC porque su NS es mucho más selectivo (DNN) y no es el probe de lumen.

Links:
- https://www.isca-archive.org/interspeech_2015/kang15_interspeech.pdf
- https://vocal.com/beamforming-2/aec-noise-suppression/
- https://www.forasoft.com/learn/audio-for-video/articles-audio/webrtc-audio-pipeline-end-to-end
- https://blog.biamp.com/the-importance-of-acoustic-echo-cancellation-part-2/
- https://switchboard.audio/hub/how-webrtc-aec3-works/ (explica RES cómo evita howling)

### Por qué NS VeryHigh tapa AEC en lumen y cómo lo evita Discord

- WebRTC NS VeryHigh = spectral Wiener ~9× atenuación estacionaria, muy agresivo — en harnessecho 440 Hz lo suprime -22 dB solo, sin reportar ERLE. No distingue eco de ruido.
- Krisp/DeepFilterNet = DNN de 2 etapas (DFNet usa ERB+gating) que preserva voz y solo suprime ruido — medido en lumen: DeepFilterNet no se usa por defecto por dañar voz en otros tests, pero es menos "máscara" que VeryHigh en tonos puros.
- Fix no es "bajar NS a None en producción" (empeora ruido), sino **separar métrica y path**: `analyze_linear_aec_output` o harness dedicado NS-off para validar AEC puro, manteniendo VeryHigh en producción pero no medir AEC a través de él.

### Double-talk & Larsen (howling)

- DTD en AEC3: `suppressor.dominant_nearend_detection` + `subband_nearend_detection`. Si echo path cambia (mover laptop), AEC3 diverge breve; RES debe contener Larsen hasta reconvergencia. Larsen observado en `mic_test` sugiere que RES no contiene — posible porque NS VeryHigh ya recortó y RES no tiene señal para estimar, o porque `anti_howling_gain` default 1.0 (sin atenuación) no actúa.
- Tunable: `suppressor.high_bands_suppression.anti_howling_activation_threshold 400.0` + `anti_howling_gain 1.0` — bajar gain a 0.2 activa atenuación anti-howling. No documentado fuera de `defaults.json5`.

---

## 5) Dos opciones concretas para lumen-voice

### Opción A — quedarnos en `webrtc-audio-processing 2.1` con tuning correcto (recomendada)

**Idea:** No cambiar AEC, cambiar cómo lo usamos y medimos. El APM actual es monolito; lo partimos conceptualmente: AEC3 path vs NS/GC2, y tuneamos AEC3 con `EchoCanceller3Config`.

**Cambios:**

1. Crear `NoiseSuppressor::with_aec3_config(cfg: EchoCanceller3Config)` que llame `Processor::with_aec3_config(48_000, cfg)` (ya existe, ver `lib.rs:153`). Mantener `apm_config_with_aec(true)` pero pasar cfg.
2. Ajustar `Cargo.toml` (ya tiene `experimental-aec3-config`), no tocar `build.rs` MSVC (contract del sketch — ese flag requiere `bundled` con clang, no MSVC; mantener gate `#[cfg(not(target_env="msvc"))]` como `FastEnhancer`).
3. Base cfg = `EchoCanceller3Config::default()` validada, luego overrides:

```rust
#[cfg(feature="experimental-aec3-config")]
pub fn tuned_aec3() -> experimental::EchoCanceller3Config {
    let mut c = experimental::EchoCanceller3Config::default();
    // Menos agresivo en nearend para preservar voz (corr 0.04 → 0.9)
    c.suppressor.dominant_nearend_detection.enr_threshold = 0.35; // 0.25→0.35
    c.suppressor.dominant_nearend_detection.snr_threshold = 20.0; // 30→20
    c.suppressor.dominant_nearend_detection.hold_duration = 70;   // 50→70
    // Un poco más de headroom para jitter cpal 20ms
    c.delay.delay_headroom_samples = 64; // 32→64
    c.delay.hysteresis_limit_blocks = 2; // 1→2
    // Cola más larga para reverberación/Larsen (costo +~10% CPU)
    c.filter.refined.length_blocks = 16; // 13→16 (≈64ms)
    c.filter.coarse.length_blocks = 16;
    // Supresor menos destructivo en voz
    c.suppressor.normal_tuning.mask_lf.enr_suppress = 0.3; // 0.4→0.3
    c.suppressor.normal_tuning.mask_hf.enr_suppress = 0.08; // 0.1→0.08
    // Anti-howling (default 1.0 = off)
    c.suppressor.high_bands_suppression.anti_howling_activation_threshold = 200.0; // 400→200
    c.suppressor.high_bands_suppression.anti_howling_gain = 0.3; // 1.0→0.3 atenúa
    // Export linear para NS separado
    c.filter.export_linear_aec_output = true;
    assert!(c.validate());
    c
}
```

4. En `apm_config_with_aec`, cuando `c.filter.export_linear_aec_output==true`, activar `NoiseSuppression { analyze_linear_aec_output: true, .. }` — ver `lib.rs:set_config` la condición `use_linear_aec_output`. Sin export, ese flag se ignora (bug #91).
5. Medir **dos harnesses**: (i) producción `NS VeryHigh` (actual, -0.7 esperado) y (ii) `NS None` (AEC puro, expect -13.5 y corr ≥0.9). El segundo es el gate real AEC. Añadir `get_stats()` logging ya en sketch (5 s wall-clock `delay_ms/erl/erle`) y per-second trend como en `aec_cancel::suppression_trend`.
6. Mantener lockstep + silence-gate + cap 50ms del sketch (diagrama `mic 960 → 2×480 render → capture`).

**Pros:**
- 0 migración, API existente, BUILDING.md intacto (solo `experimental-aec3-config` ya enabled).
- AEC3 es superior a speexdsp en calidad y drift; no se pierde NS VeryHigh ni GC2.
- `validate()` + defaults.json5 permiten iterar seguro, cada knob con test `test_aec3_configuration_tuning` como referencia (12% diferencia por 0.1→5.0 mask).

**Contras:**
- Feature experimental sin semver; cada bump 2.1→2.2 puede romper `EchoCanceller3Config` layout (usar `~2.1`).
- Requiere `bundled` (C++ build) — en CI/dev con `vendor/webrtc-audio-processing-sys` ya está, pero `msvc` necesita gate (no link).
- Tuning es empírico; sin HAL delay externo, auto-estimator seguirá siendo el límite.

**Esfuerzo:** 2-3 días (1 día cfg + 1 día harness NS-off + 1 día sweep 4-6 configs con `aec_cancel --nocapture`).

Links copiables:
- defaults.json5: https://github.com/tonarino/webrtc-audio-processing/blob/master/examples/aec-configs/defaults.json5
- experimental.rs: https://github.com/tonarino/webrtc-audio-processing/blob/master/src/experimental.rs
- lib.rs tuning test: https://github.com/tonarino/webrtc-audio-processing/blob/master/src/lib.rs (search `test_aec3_configuration_tuning`)

---

### Opción B — migrar AEC a `sonora` (pure Rust) / separar AEC del NS

**Idea:** Reemplazar solo el AEC3 C++ por Rust, manteniendo o reemplazando el resto. `sonora` es port M145 de WebRTC APM a Rust, SIMD NEON/SSE2/AVX2, 4.2µs/16k mono vs 4.0µs C++ (M4 Max). Pasa 2400 tests C++ vía FFI.

**Subopciones:**

- **B1 — `sonora-aec3` solo + keep webrtc NS/GC2:** En `lumen-voice`, `sonora-aec3::EchoCanceller3` para AEC, luego `webrtc-audio-processing` con `echo_canceller: None` solo HPF+NS+GC2. Unión: salida lineal de sonora → NS de webrtc. Requiere convertir `i16 ↔ f32` igual que audio.rs.
- **B2 — `sonora` full (AEC3+NS+AGC2):** Reemplazar `NoiseSuppressor` completo por `sonora::AudioProcessing` (API similar a webrtc, ver `crates/sonora/examples/simple.rs` y `karaoke.rs`). Elimina C++ del send path por completo, unifica tuning en `sonora-aec3::config::EchoCanceller3Config` (misma estructura que `defaults.json5` pero en Rust puro, `validate()` disponible).

Snippet `sonora` (de README + `sonora-aec3/config.rs`):

```rust
// Cargo.toml: sonora = "0.1"  (o sonora-aec3 = "0.1")
use sonora::prelude::*; // full pipeline
// o use sonora_aec3::{EchoCanceller3, config::EchoCanceller3Config};

let mut cfg = sonora_aec3::config::EchoCanceller3Config::default();
cfg.suppressor.dominant_nearend_detection.enr_threshold = 0.35;
cfg.validate(); // clamp igual que tonarino
// sonora::AudioProcessing::new(sample_rate, cfg) — ver crates/sonora/examples/simple.rs
// proceso 10ms: ap.process_capture(&mut capture_480, &render_480)
```

Para B1, el glue es manual:

```rust
// pseudocode B1
let mut aec3 = sonora_aec3::EchoCanceller3::new(48_000, cfg);
let mut ns = webrtc_ns_only_processor(); // echo_canceller: None, NS VeryHigh

for frame_20ms in capture_frames {
    let (c0, c1) = frame.split_at(480);
    // AEC por 10ms con render alineado (ring 1:1)
    let mut out0 = aec3.process(c0, render0);
    let mut out1 = aec3.process(c1, render1);
    // NS sobre salida AEC
    ns.process_capture_frame([&mut out0])?;
    ns.process_capture_frame([&mut out1])?;
}
```

**Pros:**
- Pure Rust → no `bundled`/meson/ninja, cross-compile trivial (Android/iOS ya CI), no `webrtc-audio-processing-sys` vendored, MSVC compatible.
- Config AEC3 en Rust estable (no experimental), mismo `validate()` y `multichannel_default()` pero sin FFI semver risk.
- Debug real: puedes inspeccionar `filter.export_linear_aec_output`, estados intermedios, y logging Rust.
- Benchmarks muestran paridad C++ (1.07× a 16k, 1.24× a 48k).

**Contras:**
- Crate joven (2026-07), downloads bajos, aunque validado contra upstream tests. Riesgo de divergencia si WebRTC M146+ cambia.
- Requiere auditar `sonora` tuning defaults vs `tonarino` defaults (son cercanos pero no garantizado idénticos).
- Si eliges B2 full, NS sonora ≠ NS webrtc VeryHigh — re-medición de ruido estacionario necesaria (puede ser mejor/peor; sonora NS es port Wiener también pero puede diferir en VeryHigh factor).

**Esfuerzo:** 3-5 días (1 día spike `sonora` simple.rs, 1 día B1 glue + harness, 1-2 días sweep config + validación `aec_cancel`, 1 día decisión B1 vs B2).

Links copiables:
- https://github.com/dignifiedquire/sonora
- https://github.com/dignifiedquire/sonora/blob/main/crates/sonora-aec3/src/config.rs
- https://github.com/dignifiedquire/sonora/blob/main/crates/sonora/examples/simple.rs
- https://docs.rs/crate/sonora-aec3 / https://docs.rs/crate/sonora
- https://github.com/RubyBit/aec3-rs (alternativa DAG si quieres graph)

---

## 6) Matriz decisión & próximos pasos inmediatos

| Criterio | Opción A (webrtc tuned) | Opción B (sonora) | SpeexDSP |
|---|---|---|---|
| Cancelación 440 Hz GOERTZEL | -13.5 dB NS-off (medido), -25 dB combinado | similar o mejor (mismo AEC3) | ~8-10 dB, sin RES |
| Voz corr (synthetic 120 Hz) | target 0.9 (hoy 0.35, gate no ayudó) — tunable DTD | target 0.9, más debug | peor double-talk |
| Larsen mic_test | mitigable vía anti_howling gain | inspeccionable | no |
| Build/MSVC | `experimental-aec3-config` solo non-msvc | pure Rust, msvc OK | OK pero pobre |
| Mantenimiento | tonarino activo, freedesktop 2.1 | sonora activo M145 | speexdsp 2022 |
| Riesgo | semver experimental | crate joven | calidad |

**Próximos pasos (sin edits a voz, solo investigación → ahora ejecutables):**

1. **Validación inmediata (hoy):** re-run `cargo test -p lumen-voice --test aec_cancel -- --nocapture` con `NoiseSuppression: None` temporal (flag `#[cfg(test)]` o env `AEC_NS=none`) para establecer baseline AEC puro -13.5 dB y confirmar que `with_aec3_config(tuned).suppressor.mask 0.3` lleva corr 0.35→≥0.6. Documentar trend per-second.
2. **Si A sube corr ≥0.7:** iterar DTD `enr_threshold 0.25→0.5` y `delay_headroom 32→96` en 3 configs, medir voz+sine juntos (no solo tono). Elegir ganadora y proponer PR con `#[cfg(feature="experimental-aec3-config")]` + docs.
3. **Si A no sube:** spike B1 (sonora-aec3 1 día) — `cargo add sonora-aec3`, portar `audio.rs: chain_with_aec` a `sonora_aec3::EchoCanceller3`, re-run mismo harness. Si sonora corr ≥0.9, proponer migración B1.

> Nota sobre `audio.rs` actual: `apm_config_with_aec(aec:bool)` + `Processor::new(48k)` ya implementa toggle AEC off/on (headphones-first). Cualquier opción debe respetar `aec_enabled` y `clear_tap` (session flag) — nunca alimentar render con AEC off.

---

## 7) Referencias completas (3+ con snippet)

**Ref 1 — webrtc-audio-processing 2.1 (tonarino) — tuning AEC3**
- https://github.com/tonarino/webrtc-audio-processing — README + Building (bundled/meson)
- https://github.com/tonarino/webrtc-audio-processing/blob/master/src/lib.rs — `Processor`, `set_config`, `analyze_linear_aec_output` bug #91, tests
- https://github.com/tonarino/webrtc-audio-processing/blob/master/src/experimental.rs — `EchoCanceller3Config` + `validate()` + multichannel
- https://github.com/tonarino/webrtc-audio-processing/blob/master/src/config.rs — `Config` → FFI mapping
- https://github.com/tonarino/webrtc-audio-processing/blob/master/examples/simple.rs — lockstep 10ms reference
- https://github.com/tonarino/webrtc-audio-processing/blob/master/examples/aec-configs/defaults.json5 — defaults completos AEC3
- https://freedesktop.org/software/pulseaudio/webrtc-audio-processing/ / https://gitlab.freedesktop.org/pulseaudio/webrtc-audio-processing — upstream 2.1 (enero 2025)

**Ref 2 — sonora (pure Rust WebRTC APM M145)**
- https://github.com/dignifiedquire/sonora — README, benchmarks (M4 Max 4.2µs vs 4.0µs), crates table, ejemplos simple/karaoke/recording
- https://github.com/dignifiedquire/sonora/blob/main/crates/sonora-aec3/src/config.rs — config Rust pura con `validate()`
- https://crates.io/crates/sonora / https://docs.rs/crate/sonora-aec3 — docs

**Ref 3 — SpeexDSP / aec-rs**
- https://github.com/rust-av/speexdsp-rs — bindings speexdsp (feature `sys`)
- https://github.com/thewh1teagle/aec — `aec-rs` wrapper (1.0.0, 9.6k dl, multiplatform prebuilt)
- https://docs.rs/crate/speexdsp/0.1.2 — 0% docs, 75kB, 2022-04-28
- https://www.speex.org/docs/manual/speex-manual/node7.html — API C original echo canceller

**Ref 4 — crates.io AEC**
- https://crates.io/crates/webrtc-audio-processing (78k) / https://crates.io/crates/webrtc-audio-processing-sys
- https://crates.io/crates/aec-rs / https://crates.io/crates/fdaf-aec (~400) / https://crates.io/crates/speexdsp
- https://crates.io/crates/sonora-aec3 / https://crates.io/crates/aec3 (RubyBit)

**Ref 5 — Discord/Krisp & pipeline AEC→NS**
- https://discord.com/blog/how-discord-handles-two-and-half-million-concurrent-voice-users-using-webrtc — Discord WebRTC
- https://support.discord.com/hc/en-us/articles/360040843952-Krisp-FAQ / https://krisp.ai/discord/ — Krisp DNN NS (AEC→Krisp→AGC)
- https://switchboard.audio/hub/how-webrtc-aec3-works/ — cómo AEC3 funciona (linear + RES, howling prev)
- https://www.forasoft.com/learn/audio-for-video/articles-audio/webrtc-audio-pipeline-end-to-end — orden HPF→AEC→NS→AGC2
- https://www.isca-archive.org/interspeech_2015/kang15_interspeech.pdf — joint AEC+NS estimation (AEC antes de NS)
- https://vocal.com/beamforming-2/aec-noise-suppression/ — por qué NS antes rompe AEC

---

*Entregable: solo investigación, no edits a `crates/lumen-voice/src/audio.rs` ni `client.rs`. Siguiente build elegir A o B y abrir PR con harness NS-off como gate AEC puro.* 
