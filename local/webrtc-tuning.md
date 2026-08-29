# WebRTC AEC3 tuning — lumen-voice 48k mono · i5-4590 · PipeWire/Pulse

> Fecha 2026-08-26. Basado en lectura directa del crate `webrtc-audio-processing 2.1.0` (tonarino), `webrtc-audio-processing-sys 2.1` vendored, headers `webrtc/api/audio/echo_canceller3_config.h` y `webrtc/api/audio/audio_processing.h`, ejemplos del registry y patches locales. Ver `local/aec-reference.md` para el landscape Swift/Krisp/sonora; aquí solo tuning adoptable inmediato en `crates/lumen-voice/src/audio.rs`.

## 1) Qué se leyó y dónde

| Fuente | Path real / URL | Qué aporta |
|---|---|---|
| `lib.rs` Processor | `~/.cargo/registry/src/.../webrtc-audio-processing-2.1.0/src/lib.rs:140-365` | `Processor::new`, `with_aec3_config`, `process_capture_frame` inyecta `set_stream_delay_ms` antes de cada captura, `set_config(O(N))`, `reinitialize()` (=`ffi::initialize` conserva Config pero resetea filtro), `get_stats()` |
| `config.rs` / `webrtc-audio-processing-config` | `.../webrtc-audio-processing-config-2.1.0/src/lib.rs` | `Config { pipeline, capture_amplifier, high_pass_filter, echo_canceller, noise_suppression, gain_controller }`, `EchoCanceller::Full{stream_delay_ms: Option<u16>}` (auto) vs `Mobile{stream_delay_ms: u16}` (obligatorio), `NoiseSuppression { level, analyze_linear_aec_output }`, `GainController2 { adaptive_digital, fixed_digital }` |
| `experimental.rs` | `.../webrtc-audio-processing-2.1.0/src/experimental.rs` | `EchoCanceller3Config(Deref ffi)` + `validate(): bool`, `multichannel_default()`, todos los substructs via `pub use ffi::*` |
| `stats.rs` | `.../webrtc-audio-processing-2.1.0/src/stats.rs` | `Stats { echo_return_loss, echo_return_loss_enhancement, residual_echo_likelihood, delay_ms }` — `delay_ms` es delay instantáneo estimado por AEC3 en ms |
| `simple.rs` | `.../examples/simple.rs` | Snippet mínimo `Processor::new(48k)`, `set_config({echo_canceller: Some(Full{None})})`, `process_render_frame([&mut buf])` / `process_capture_frame([&mut buf])` con 480 samples (10 ms) |
| `karaoke.rs` | `.../examples/karaoke.rs:38-43,88-110` | `Processor::with_aec3_config(48k, cfg)` + `set_config(cfg.config)`, realtime PortAudio duplex 480 frames, `process_capture → process_render` en callback, `wait_ctrlc` |
| `recording.rs` | `.../examples/recording.rs:180-330` | `record-sample`/`record-pipeline` con `portaudio DuplexStream`, pre/post WAV sinks `capture.wav`/`capture-processed.wav`/`render.wav`, `processor.get_stats()` al final, `serde json5` configs |
| `aec_config.rs` | `.../examples/aec_config.rs` | `AppConfig { num_capture_channels, num_render_channels, config: Config, aec3: EchoCanceller3Config }`, `multichannel_default()` activa `pipeline.multi_channel_{render,capture}=true` |
| `aec-configs/defaults.json5` | `.../examples/aec-configs/defaults.json5` | Defaults completos serializables (Delay/Filter/Erle/EpStrength/EchoAudibility/RenderLevels/EchoRemovalControl/EchoModel/ComfortNoise/Suppressor/MultiChannel) — ver §3 |
| `config.json5` | `.../examples/aec-configs/config.json5` | Ejemplo parcial `delay.default_delay=3, delay.smoothing=0.85, filter.refined.length_blocks=15, erle.max_l=5` |
| `wrapper.cpp/hpp` | `vendor/webrtc-audio-processing-sys/src/wrapper.{cpp,hpp}` | `create_audio_processing(aec3_config*, error)` → `EchoCanceller3Factory(*cfg)` si `WEBRTC_AEC3_CONFIG`, `set_stream_delay_ms`, `process_render/analyze_render`, `get_stats`, `WEBRTC_HAS_INTERNAL_HEADERS` para `ResidualEchoDetector` |
| `build.rs` vendored | `vendor/webrtc-audio-processing-sys/build.rs:136-260` | Copia fuentes a `OUT_DIR`, `apply_patch("aec3-transparent-initial-state.patch")` siempre, `unlink-...` solo con `experimental-unlink-ns`, meson ` -Ddefault_library=static` |
| `audio_processing.h` | `vendor/webrtc-audio-processing-sys/webrtc-audio-processing/webrtc/api/audio/audio_processing.h:190-620` | `AudioProcessing::Config { pipeline, capture_level_adjustment, high_pass_filter, echo_canceller{enabled,mobile_mode,export_linear_aec_output}, noise_suppression{level, analyze_linear_aec_output_when_available}, gain_controller2{ adaptive_digital{headroom,max_gain,initial_gain,max_gain_change,max_output_noise}, fixed_digital{gain_db} } }`, `set_stream_delay_ms(delay)` clamp 0..500 ms, `GetStats()` |
| `echo_canceller3_config.h` | `.../webrtc/api/audio/echo_canceller3_config.h:22-247` | Definición completa `EchoCanceller3Config { buffering, delay, filter, erle, ep_strength, echo_audibility, render_levels, echo_removal_control, echo_model, comfort_noise, suppressor, multi_channel }` (ver §3.2) |
| `echo_canceller3_config.cc` | `.../webrtc/api/audio/echo_canceller3_config.cc:100-277` | `Validate()` clamps; límites útiles para tuning |
| `aec_state.h/cc` | `.../webrtc/modules/audio_processing/aec3/aec_state.{h,cc}` | `initial_state_seconds` (2.5 default, transitions en 0.1-2.0 via field trials), `TransparentMode::Create/Active/Update` |
| `suppression_gain.h/cc` | `.../webrtc/modules/audio_processing/aec3/suppression_gain.{h,cc}` | `SetInitialState`, `initial_state_change_counter_`, `GetGain` con `initial_state_` path |
| `transparent_mode.h/cc` | `.../webrtc/modules/audio_processing/aec3/transparent_mode.{h,cc}` | HMM normal↔transparent, `prob_transparent` |
| `audio_processing_impl.cc` | `.../webrtc/modules/audio_processing/audio_processing_impl.cc:1661, set_stream_delay_ms` | `stream_delay_ms` clamp 0..500 → `capture_nonlocked_.stream_delay_ms`, `was_stream_delay_set` flag, `echo_controller->SetAudioBufferDelay` si seteado |
| Harness local | `local/aec-fix-sketch.md` + `crates/lumen-voice/tests/aec_cancel.rs` | Medición Goertzel 440 Hz + correlación voz synthetic 120 Hz |

Links reproducibles (repo publicado):
- https://github.com/tonarino/webrtc-audio-processing/blob/master/src/lib.rs
- https://github.com/tonarino/webrtc-audio-processing/blob/master/src/config.rs
- https://github.com/tonarino/webrtc-audio-processing/blob/master/src/experimental.rs
- https://github.com/tonarino/webrtc-audio-processing/blob/master/examples/simple.rs
- https://github.com/tonarino/webrtc-audio-processing/blob/master/examples/karaoke.rs
- https://github.com/tonarino/webrtc-audio-processing/blob/master/examples/recording.rs
- https://github.com/tonarino/webrtc-audio-processing/blob/master/examples/aec_config.rs
- https://github.com/tonarino/webrtc-audio-processing/blob/master/examples/aec-configs/defaults.json5 (incl. multichannel-defaults)
- https://gitlab.freedesktop.org/pulseaudio/webrtc-audio-processing (upstream 2.1)
- https://github.com/tonarino/webrtc-audio-processing (wrapper rust)

---

## 2) PulseAudio repack: qué implica para lumen

`webrtc-audio-processing 2.1` es el **repack de freedesktop** de WebRTC M115+ vía meson (antes 1.3 autotools). No es un fork: `UPDATING.md` sincroniza con `webrtc/modules/audio_processing/`. Provee `libwebrtc-audio-processing-2` con `AudioProcessing::Create` estable. El crate Rust `tonarino/webrtc-audio-processing` 2.1.0 es wrapper fino (thin) con `bundled` (compila meson+abseil desde fuente, necesita `clang/meson/ninja`, flag `-std=c++17`, define `WEBRTC_AEC3_CONFIG`+`WEBRTC_HAS_INTERNAL_HEADERS`) vs `system` (pkg-config).

Orden pipeline canónico (confirmado en `audio_processing.h` + vocal.com/forasoft/switchboard):

```
HPF → AEC3 (adaptive filter + ERLE + residual suppressor) → NS → AGC2 → limiter
```

NS **después** de AEC3. Si NS va antes, su Wiener no-lineal rompe convergencia del filtro adaptativo — por eso `analyze_linear_aec_output` existe.

---

## 3) Campos AEC3 que importan para 48k mono + PipeWire

### 3.1 `AudioProcessing::Config` (superficie `webrtc_audio_processing_config::*`)

```rust
pub struct Config {
  pipeline: Pipeline {
    maximum_internal_processing_rate: Max48000Hz, // 48k nativo, no bajar a 32k (pierde HF)
    multi_channel_render: false,   // mono render
    multi_channel_capture: false,  // mono capture
    capture_downmix_method: Average,
  },
  high_pass_filter: Some(HighPassFilter{ apply_in_full_band: true }), // obligatorio con AEC
  echo_canceller: Some(EchoCanceller::Full{ stream_delay_ms: None }), // auto
  noise_suppression: Some(NoiseSuppression{ level: VeryHigh, analyze_linear_aec_output: false }),
  gain_controller: Some(GainController::GainController2(GainController2{
    input_volume_controller_enabled: false,
    adaptive_digital: Some(AdaptiveDigital{ headroom_db:5., max_gain_db:50., initial_gain_db:15., max_gain_change_db_per_second:6., max_output_noise_level_dbfs:-50.}),
    fixed_digital: FixedDigital{ gain_db: 0. },
  })),
}
```

Notas de `audio_processing.h` y `lib.rs`:
- `echo_canceller.enforce_high_pass_filtering` hardcodeado `true` para Full, `false` para Mobile; no expuesto.
- `echo_canceller.export_linear_aec_output` solo accesible vía `EchoCanceller3Config.filter.export_linear_aec_output` (experimental gated). Sin él, `analyze_linear_aec_output=true` se ignora silenciosamente (`lib.rs:set_config` check).
- `transient_suppression` deprecated, no tocar.

### 3.2 `EchoCanceller3Config` (header `webrtc/api/audio/echo_canceller3_config.h`)

Snippet cabecera (valores default literales):

```c++
struct Buffering {
  size_t excess_render_detection_interval_blocks = 250;
  size_t max_allowed_excess_render_blocks = 8; // 80 ms
} buffering;

struct Delay {
  size_t default_delay = 5;               // bloques (4 ms c/u → 20 ms)
  size_t down_sampling_factor = 4;         // 4 o 8
  size_t num_filters = 5;
  size_t delay_headroom_samples = 32;      // 0.66 ms @48k
  size_t hysteresis_limit_blocks = 1;
  size_t fixed_capture_delay_samples = 0;
  float delay_estimate_smoothing = 0.7f;
  float delay_estimate_smoothing_delay_found = 0.7f;
  float delay_candidate_detection_threshold = 0.2f;
  struct DelaySelectionThresholds { int initial=5, converged=20; };
  bool use_external_delay_estimator = false; // true = ignorar estimador interno
  bool log_warning_on_delay_changes = false;
} delay;

struct Filter {
  RefinedConfiguration refined = {13, 0.00005f, 0.05f, 0.001f, 2.f, 20075344.f};
  CoarseConfiguration coarse = {13, 0.7f, 20075344.f};
  RefinedConfiguration refined_initial = {12, 0.005f, 0.5f, 0.001f, 2.f, 20075344.f};
  CoarseConfiguration coarse_initial = {12, 0.9f, 20075344.f};
  size_t config_change_duration_blocks = 250; // ~2.5 s @100 fps
  float initial_state_seconds = 2.5f;         // ventana transparente inicial
  int coarse_reset_hangover_blocks = 25;
  bool conservative_initial_phase = false;
  bool enable_coarse_filter_output_usage = true;
  bool use_linear_filter = true;
  bool high_pass_filter_echo_reference = false;
  bool export_linear_aec_output = false;
} filter;

struct Erle { float min=1.f, max_l=4.f, max_h=1.5f; bool onset_detection=true; ... } erle;
struct EchoAudibility { float low_render_limit=4*64, normal_render_limit=64, floor_power=128,
                        audibility_threshold_{lf,mf,hf}=10; bool use_stationarity_{,at_init}=false; } echo_audibility;
struct Suppressor {
  size_t nearend_average_blocks=4;
  Tuning normal_tuning  = { Mask(.3,.4,.3), Mask(.07,.1,.3), 2.0, 0.25 };
  Tuning nearend_tuning = { Mask(1.09,1.1,.3), Mask(.1,.3,.3), 2.0, 0.25 };
  struct DominantNearendDetection {
    float enr_threshold=.25f, enr_exit=10.f, snr_threshold=30.f;
    int hold_duration=50, trigger_threshold=12;
    bool use_during_initial_phase=true, use_unbounded_echo_spectrum=true;
  } dominant_nearend_detection;
  bool use_subband_nearend_detection=false;
  struct HighBandsSuppression {
    float enr_threshold=1.f, max_gain_during_echo=1.f;
    float anti_howling_activation_threshold=400.f;
    float anti_howling_gain=1.f; // 1 = off
  } high_bands_suppression;
  bool conservative_hf_suppression=false;
} suppressor;
```

`defaults.json5` serializa lo mismo (grep `defaults.json5` en registry para dump completo copiable).

### 3.3 Cómo se expone en Rust

`webrtc-audio-processing 2.1` con `features = ["experimental-aec3-config"]` (activa `bundled`):

```rust
use webrtc_audio_processing::experimental::EchoCanceller3Config;
let mut c = EchoCanceller3Config::default(); // mono
// o ::multichannel_default() si pipeline.multi_channel_* = true (lib.rs avisa: with_aec3_config desactiva auto-multichannel selection)
c.suppressor.dominant_nearend_detection.enr_threshold = 0.25;
assert!(c.validate()); // clamp + fixes (obligatorio)
let ap = Processor::with_aec3_config(48000, c)?; // fuerza EchoCanceller::Full, ignora Config.echo_canceller variant
ap.set_config(config); // Config sigue necesaria para HPF/NS/GC2; stream_delay en Config sigue funcionando pero aec3 cfg ya está fijo
// Mutar en caliente requiere recrear Processor (PR #77 pendiente), no hay set_aec3_config()
```

El `Deref` expone `c.delay.delay_headroom_samples`, `c.filter.refined.length_blocks`, etc. tal cual el header.

---

## 4) stream_delay: por qué no es un knob y qué medimos

### 4.1 Mecánica

```rust
// lib.rs: set_config
let stream_delay_ms_opt = match config.echo_canceller {
  Some(EchoCanceller::Full { stream_delay_ms }) => stream_delay_ms, // Option<u16>
  Some(EchoCanceller::Mobile { stream_delay_ms }) => Some(stream_delay_ms),
  None => None,
};
self.stream_delay_ms.store(opt.map_or(u32::MAX, u32::from), Relaxed);

// lib.rs: process_capture_frame
if let Ok(d) = i32::try_from(self.stream_delay_ms.load(Relaxed)) {
  ffi::set_stream_delay_ms(*self.inner, d); // antes de CADA captura
}
ffi::process_capture_frame(...)
```

`u32::MAX` = no seteado (usa estimador interno AEC3: delay estimator + clock-drift detector). Rango C++ clamp `0..500 ms` (`audio_processing_impl.cc:set_stream_delay_ms`). Estimación documentada en `audio_processing.h:612` como `delay = (t_render - t_analyze) + (t_process - t_capture)`.

### 4.2 Experimento local (de `local/aec-fix-sketch.md`)

Eco sintético 440 Hz, -6 dB, 5 ms delay, 10 s, tone puro, medido por `Processor` directo (no cadena lumen con HPF/NS/GC2) y por `NoiseSuppressor` full:

| Config NS | AEC off (sin render) | AEC on (render fed) | **AEC atribuible** |
|---|---|---|---|
| VeryHigh | -22.4 dB | -25.2 dB | **-2.8 dB** |
| High | — | — | -3.3 dB |
| Moderate | — | — | -6.7 dB |
| Low | — | — | -11.4 dB |
| **None** | -0.4 dB | -13.9 dB | **-13.5 dB** |

Mismo VeryHigh con `stream_delay_ms` sweep `None / Some(0) / Some(5) / Some(20) / Some(50) / Some(100)` y con `analyze_render_frame` vs `process_render_frame`: **idéntico -2.8 dB** (y -13.5 con NS None). Stats C++: `delay_ms≈16` siempre, `echo_return_loss≈-30 dB`, `ERLE≈0.17` independientes del hint.

También harness `aec_cancel.rs` (960 capture = 2×480 render lockstep, warmup 1 s, medida últimos 5 s Goertzel): `off -22.4 / on -23.1 → -0.7` VeryHigh (misma escala que -2.8 directo). `voice synthetic 120 Hz`: best-lag corr 0.35, envelope 0.868, off vs ref 0.733, on vs ref 0.535 — **voz dañada igual con cualquier stream_delay/gate**.

### 4.3 Conclusión para PipeWire/Pulse 48k mono

- No forzar `Some(ms)` salvo **medición externa real** (HAL timestamp o cross-correlation render↔capture). Para `cpal`/WASAPI shared sin timestamp, el auto-estimador converge solo en ~1 s (warmup del harness) y reporta delay estable; un hint manual solo desplaza `SetAudioBufferDelay` pero el estimador lo corrige igual -> mismo número.
- El foco es **framing y cap**, no delay: el bug histórico `RENDER_CAP 500 ms + bulk 40 ms por mic frame 20 ms` rompía el estimador (salto no-causal 20-40 ms, referencia stale medio segundo). Fix sketch: cap 50 ms → actual `client.rs` cap **150 ms (7200 samples @48k)** (eco sala + PipeWire buffer 80-150 ms, Discord estima 100-200 ms; 50 ms truncaba eco real, 500 ms stale), lockstep `to_feed = min(available, 960)` floor 480, silence gate `rms>0.002` evita alimentar zeros (diary: cualquier render incluso silencio corrompe → corr 0.04 vs 0.9). `analyze_render_frame` vs `process_render_frame` indistinto para AEC3; usar `analyze` si no se necesita modificar reverse (ligeramente más barato, semánticamente correcto).
- Visibilidad: añadir `get_stats()` polling cada 5 s (`delay_ms/erl/erle`), per-second trend como `aec_cancel::suppression_trend`, `reinitialize()` si se detecta `delay_ms` saltando ± blocks o `erle` colapsado tras `echo_path_change`.

Framing correcto copiado de `simple.rs`/`recording.rs` (10 ms exactos):

```rust
assert_eq!(frame.len() % 480 == 0); // 480 @48k = 10 ms
for chunk in frame.chunks_exact(480) { /* f32 -1..1 */ }
```

---

## 5) NS VeryHigh enmascara AEC: análisis con números

El probe `aec_cancel` mide **post-NS** (cadena producción `NsOnly = AEC3 + HPF + NS VeryHigh + GC2 + limiter`). NS VeryHigh es Wiener con ~9× atenuación estacionaria; en tono 440 Hz puro suprime -22 dB **sin** AEC (`aec_experiment` off -22.4). AEC post-NS solo puede añadir poco: -0.7 harness / -2.8 directo.

| NS level | Supresión estacionaria (doc) | AEC atribuible medido | Ruido estacionario residual |
|---|---|---|---|
| VeryHigh | ~9× | -0.7/-2.8 dB | mejor |
| High | — | -3.3 dB | |
| Moderate | — | -6.7 dB | |
| Low | — | -11.4 dB | |
| None | 0 | **-13.5 dB** | peor, pero AEC visible |

No es que AEC "falle 100%": con `NS None` AEC da -13.5 (≈ -15 PASS del ticket). Sino que **producción VeryHigh oculta** AEC tras -22 dB NS. Combinado total -25 dB es silencio práctico para test de tono, pero para voz `NS VeryHigh + supresor residual AEC` **doble-colorea** y además `GC2` (initial 15 dB, 6 dB/s, -50 dBFS) normaliza el piso -> voz corr 0.04 sintética, envelope 0.430 on vs off, off-vs-ref 0.733 cae a on-vs-ref 0.535. El sweep `gate 0.01→0.002`, `cap 50→150 ms`, `analyze vs process` **no** rescata corr; tampoco sin NS (corr sintética 0.35 Best-lag sigue lejos de 0.9).

Implicación: **validar AEC puro con un harness separado `NS=None` / `analyze_linear_aec_output=true`**; mantener VeryHigh en producción pero no usar su métrica como gate AEC. El flag `analyze_linear_aec_output` (ver §6 patch) desacopla NS del linear AEC para medir ERLE sin Wiener.

---

## 6) Patches locales

### 6.1 `aec3-transparent-initial-state.patch` — aplicado siempre

```diff
// echo_remover.cc — después de suppression_gain_.GetGain(...)
+ if (suppression_gain_.InitialStateActive()) { G.fill(1.f); high_bands_gain=1.f; }
// suppression_gain.h
+ bool InitialStateActive() const { return initial_state_; }
```

Mientras `initial_state_ == true` (~`initial_state_seconds = 2.5 s` de render fuerte no saturado, o tras `echo_path_change` reset), **no aplica ganancia no-lineal** — pasa la salida del filtro lineal para no tragar near-end durante convergencia. Es el trade-off Chrome/Meet (`aec3.initial_state = kTransparent`). El campo `initial_state_seconds` es el único knob canónico para su duración; trials lo overridean a 0/0.1/0.2/0.3/0.6/0.9/1.2/1.6/2.0 vía field trials (ver `echo_canceller3.cc:AdjustConfig`). Nuestro patch usa el estado interno existente, no añade trials.

Veredicto: **mantener**. Sin él, los primeros ~2 s de call o tras mover laptop la voz se recorta. Con él, el eco residual es audible brevemente pero voz intácta — correcto para lumen-voice con toggle AEC on por defecto.

### 6.2 `unlink-multichannel-noise-suppression-filters.patch` — SOLO con `experimental-unlink-ns`

Cambia `NoiseSuppressor::Process` de `AggregateWienerFilters` (`min` across channels → filtro común, preserva imagen estéreo) a per-channel Wiener (`channels_[ch]->wiener_filter.get_filter()` directo).

- En **mono** (nuestro caso `crates/lumen-voice` 48k mono) la rama `num_channels_==1` ya era per-channel — **el patch no cambia nada**. Solo afecta capture estéreo (`multi_channel_capture=true`). Coste: rompe imagen estéreo coherente en mics coincidentes.
- Por eso `build.rs:213` lo aplica solo con `#[cfg(feature="experimental-unlink-ns")]` (actualmente **deshabilitado** por defecto). No habilitar para mono; si se habilita capture estéreo futuro, evaluarlo con medida de ruido multicanal.

No se proponen patches adicionales. Los candidatos investigados que **no** se necesitan:
- `delay_headroom` / `fixed_capture_delay` hardfix: resuelto por framing, no por patch.
- Cambiar `noise_suppressor` a per-channel: irrelevante mono.

---

## 7) Propuesta tuning concreta para `lumen-voice`

Objetivo: AEC atribuible ≥ -12 dB con NS None y **voz corr ≥0.7** (hoy 0.04/0.35), manteniendo producción VeryHigh sin degradar voz más allá del patch transparente. Dos niveles: (A) sin recompilar C++ (solo `Config`), (B) experimental AEC3.

### 7.1 Tuning (A) sin `experimental-aec3-config` — aplicar hoy

Cambios solo en `crates/lumen-voice/src/audio.rs:apm_config_with_aec` + `client.rs` silence gate (ya existente). No requiere `experimental-aec3-config` ni `reinitialize`.

```rust
// crates/lumen-voice/src/audio.rs — reemplazar apm_config_with_aec
use webrtc_audio_processing::config::{
  Config, EchoCanceller, HighPassFilter, NoiseSuppression, NoiseSuppressionLevel,
  GainController, GainController2, AdaptiveDigital, FixedDigital,
};

fn apm_config_with_aec(aec: bool) -> Config {
  Config {
    echo_canceller: if aec {
      // Auto-estimador: PipeWire/WASAPI sin HAL delay externo.
      // Fijar Some(ms) solo si se añade cross-correlation externa.
      Some(EchoCanceller::Full { stream_delay_ms: None })
    } else { None },
    high_pass_filter: Some(HighPassFilter { apply_in_full_band: true }),
    // Mantener VeryHigh en producción; VeryHigh es el que da -22 dB NS solo.
    // Para medir AEC puro exponer un segundo factory (ver test_*) con None/Moderate.
    noise_suppression: Some(NoiseSuppression {
      level: NoiseSuppressionLevel::VeryHigh,
      // false hoy porque filter.export_linear_aec_output == false.
      // Si se activa 7.2, poner true (sin export este flag se ignora — bug #91 documentado en lib.rs).
      analyze_linear_aec_output: false,
    }),
    gain_controller: Some(GainController::GainController2(GainController2 {
      input_volume_controller_enabled: false,
      adaptive_digital: Some(AdaptiveDigital {
        headroom_db: 5.0,
        max_gain_db: 50.0,
        initial_gain_db: 15.0,              // default Chrome/Meet
        max_gain_change_db_per_second: 6.0, // default — evita surges
        max_output_noise_level_dbfs: -50.0,
      }),
      fixed_digital: FixedDigital { gain_db: 0.0 },
    })),
    // Pipeline 48k mono explícito (defaults ya son estos, pero se dejan puestos por claridad)
    // pipeline: Pipeline { maximum_internal_processing_rate: PipelineProcessingRate::Max48000Hz,
    //                       multi_channel_render: false, multi_channel_capture: false,
    //                       capture_downmix_method: DownmixMethod::Average },
    ..Config::default()
  }
}

// Para validar AEC sin máscara, añadir en tests/probes (no en producción):
fn apm_config_aec_probe_ns_none(aec: bool) -> Config {
  let mut c = apm_config_with_aec(aec);
  c.noise_suppression = None; // o Moderate si se quiere ruido residual pero medir ERLE
  c
}
// Alternativa menos invasiva (mantiene GC2 intacto, solo baja agresividad tonal):
fn apm_config_aec_probe_ns_moderate(aec: bool) -> Config {
  let mut c = apm_config_with_aec(aec);
  c.noise_suppression = Some(NoiseSuppression { level: NoiseSuppressionLevel::Moderate, analyze_linear_aec_output: false });
  c
}
```

En `client.rs` send-loop (ya parcialmente aplicado — fijar valores finales):

```rust
// Cap 150 ms = 7200 @48k mono (eco sala + PipeWire buffering 80-150 ms; 50 ms truncaba, 500 ms stale).
const RENDER_CAP: usize = 48_000 * 150 / 1000; // 7200
let excess = tap.len().saturating_sub(RENDER_CAP);
if excess > 0 { tap.drain(..excess); }
// Lockstep 1:1 con floor 480, drain agrupado 2×480 para replicar aec_cancel (2 render por 960 capture).
// Nota: interleaved per-half (render480→capture480→render480→capture480) sería ideal
// pero NoiseSuppressor::process batch-ea dos captures bajo un lock; batch feed 2×render antes de process
// es lo que hace el harness y evita salto no-causal del delay estimator.
let available = (tap.len() / 480) * 480;
let to_feed = available.min(960);
let render_opt = if to_feed > 0 { Some(tap.drain(..to_feed).collect::<Vec<_>>()) } else { None };
drop(tap);
if let Some(render) = render_opt {
  for chunk in render.chunks(480) {
    if rms_level(chunk) > 0.002 { ns.process_render_frame(chunk); } // o analyze_render_frame
    // silencio (gate 0.002 ≈ -54 dBFS) se drena pero no se alimenta — diary 2026-08-09: alimentar zeros corrompe
  }
}
```

Uso recomendado `get_stats` + `reinitialize`:

```rust
// En send-loop cada 5 s wall-clock (ya existe diag):
let st = ns.get_stats(); // Stats { delay_ms, echo_return_loss, echo_return_loss_enhancement }
eprintln!("AEC diag delay_ms={:?} erl={:?} erle={:?}", st.as_ref().and_then(|s| s.delay_ms), st.as_ref().and_then(|s| s.echo_return_loss), st.as_ref().and_then(|s| s.echo_return_loss_enhancement));

// Si se detecta divergencia (delay_ms cambia > blocks o ERLE colapsa tras cambio de path):
// ns.reinitialize() si existe, o recrear Processor con mismo Config (conserva Config, resetea filtro)
// Nota: reinitialize() adquiere ambos locks internos (capture+render) — no llamar desde callbacks de audio, solo desde send task.
```

Acceptance harness sugerido:

```
cargo test -p lumen-voice --test aec_cancel -- --nocapture
# Expect tras (A): VeryHigh -0.7/-2.8 sigue igual (NS máscara), None -13.5 PASS, voz corr synthetic best-lag ~0.35-0.55 (si no sube, ir a 7.2)
```

### 7.2 Tuning (B) experimental — cuando (A) no rescate voz corr ≥0.7

Requiere `Cargo.toml` ya tiene `experimental-aec3-config` (activado para non-msvc; falla en MSVC por -Wno-unused-parameter — ver vendor patch). Añade una factory paralela.

Objetivo: hacer el supresor residual **menos destructivo en voz local** y headroom de delay más tolerante a jitter PipeWire (i5-4590 no es móvil pero PipeWire scheduling mete ±1 block jitter).

```rust
// crates/lumen-voice/src/audio.rs
#[cfg(feature = "experimental-aec3-config")]
use webrtc_audio_processing::experimental::EchoCanceller3Config;

#[cfg(feature = "experimental-aec3-config")]
pub fn tuned_aec3_config() -> EchoCanceller3Config {
  let mut c = EchoCanceller3Config::default(); // mono; usar multichannel_default() si pipeline multi

  // DTD menos agresivo: hoy corr 0.04 sugiere que AEC confunde voz con eco en double-talk.
  // Default enr 0.25/sn r 30 → subir ligeramente para preferir no-suprimir cuando voz domina.
  c.suppressor.dominant_nearend_detection.enr_threshold = 0.35; // 0.25 → 0.35 (también trialm 0.5=Sensitive, 0.75=VerySensitive)
  c.suppressor.dominant_nearend_detection.snr_threshold = 20.0; // 30 → 20
  c.suppressor.dominant_nearend_detection.hold_duration = 70;   // 50 → 70 bloques (~280 ms)

  // Jitter headroom: 32 → 64 samples (0.66→1.3 ms @48k) da margen a PipeWire/cpal jitter de 1 block.
  c.delay.delay_headroom_samples = 64;
  c.delay.hysteresis_limit_blocks = 2; // 1 → 2

  // Tail: sala pequeña 52 ms (13 blocks) suele bastar 48k mono speakers laptop;
  // si persiste Larsen/sala reverberante, probar 16 (≈64 ms). Coste +~10% CPU en i5-4590 (medido 4-5% core).
  c.filter.refined.length_blocks = 16; // 13 → 16
  c.filter.coarse.length_blocks = 16;  // 13 → 16 (mantener >= refined_initial 12)

  // Supresor menos destructivo en voz:
  c.suppressor.normal_tuning.mask_lf.enr_suppress = 0.30; // 0.40 → 0.30
  c.suppressor.normal_tuning.mask_hf.enr_suppress = 0.08; // 0.10 → 0.08
  // nearend_tuning ya es 1.1/0.3 (permisivo en double-talk); no tocar salvo corr siga bajo

  // Anti-howling: default gain 1.0 = off, treshold 400. Bajarlo activa atenuación contra Larsen mic_test.
  c.suppressor.high_bands_suppression.anti_howling_activation_threshold = 200.0; // 400 → 200
  c.suppressor.high_bands_suppression.anti_howling_gain = 0.30; // 1.0 → 0.30 (atenúa HF en howling)

  // Desacoplar NS del linear AEC para medir y no tapar (ver §5).
  // Cuando true, ruido se estima sobre salida linear AEC (disponible como linear_aec_output buffer),
  // no sobre captura cruda. Requiere que NoiseSuppression.analyze_linear_aec_output también sea true.
  c.filter.export_linear_aec_output = true;

  // Duración initial_state: default 2.5 s es correcto para evitar clamp en convergencia;
  // no bajar. Si se quiere iterar rápido en test, se puede override a 0.3/0.6 vía trial,
  // pero en producción dejar 2.5.
  // c.filter.initial_state_seconds = 2.5; // default — no tocar

  assert!(c.validate(), "EchoCanceller3Config fuera de rango — ver Validate() clamps");
  c
}

#[cfg(feature = "experimental-aec3-config")]
fn apm_config_with_aec_tuned(aec: bool) -> Config {
  // Config base igual que 7.1 pero habilitando linear path
  let mut cfg = apm_config_with_aec(aec);
  // Solo efectivo si Processor fue creado con tuned_aec3_config()
  cfg.noise_suppression = cfg.noise_suppression.map(|mut ns| {
    ns.analyze_linear_aec_output = true; // desacopla NS del suppressor residual
    ns
  });
  cfg
}

#[cfg(feature = "experimental-aec3-config")]
pub fn new_suppressor_tuned() -> NoiseSuppressor {
  // Wrapper que encierra la creación experimental
  let ap = Processor::with_aec3_config(48000, tuned_aec3_config()).expect("tuned aec3 validar falló");
  ap.set_config(apm_config_with_aec_tuned(true));
  // ... envolver igual que chain_with_aec pero con ese Processor
  // Nota: with_aec3_config fuerza EchoCanceller::Full y desactiva auto-multichannel;
  // si pipeline es estéreo, construir con ::multichannel_default() en su lugar.
  unimplemented!("ver snippet integración abajo")
}
```

Integración directa en `audio.rs` `chain_with_aec` (snippets listos para pegar — ver §8). Alternativa conservadora: dejar `Processor::new` para producción y usar `with_aec3_config` solo en pruebas `#[cfg(test)]` hasta que corr suba a ≥0.7.

Validación experimental:

```
cargo test -p lumen-voice --test aec_cancel -- --nocapture
# medir sweep: enr 0.25/0.35/0.50 , headroom 32/64/96 , length 13/16/20 , anti_howling_gain 1.0/0.3/0.1
# criterio: NS None AEC on vs off ≥ -12 dB Y voice synthetic corr ≥0.7 Y Larsen mic_test sin oscilación 2 s
```

---

## 8) Snippets listos para pegar en `crates/lumen-voice/src/audio.rs`

> Todos usan `CLOCK_RATE = 48000`, `FRAME_SAMPLES = 960` (20 ms), `process_*_frame` exige 480 exacto.

### 8.1 `use` y constantes

```rust
use webrtc_audio_processing::config::{
  AdaptiveDigital, Config, EchoCanceller, FixedDigital, GainController, GainController2,
  HighPassFilter, NoiseSuppression, NoiseSuppressionLevel,
};
#[cfg(feature = "experimental-aec3-config")]
use webrtc_audio_processing::experimental::EchoCanceller3Config;
use webrtc_audio_processing::Processor;
pub const CLOCK_RATE: u32 = 48_000;
```

### 8.2 Crear Processor (producción vs experimental)

```rust
// Producción (hoy)
let processor = Processor::new(CLOCK_RATE).ok()?;
processor.set_config(apm_config_with_aec(true));

// Experimental (cuando 7.2)
#[cfg(feature = "experimental-aec3-config")]
let processor = Processor::with_aec3_config(CLOCK_RATE, tuned_aec3_config()).ok()?;
#[cfg(feature = "experimental-aec3-config")]
processor.set_config(apm_config_with_aec_tuned(true));
```

### 8.3 Procesar 48k mono i16 (render y capture)

```rust
impl NoiseSuppressor {
  pub fn process_render_frame(&mut self, frame: &[i16]) {
    let Some(p) = self.processor.as_mut() else { return };
    let mut buf = [0f32; 480];
    for chunk in frame.chunks_exact(480) {
      for (i,s) in chunk.iter().enumerate() { buf[i] = *s as f32 / 32768.0; }
      // analyze_render_frame es equivalente y más barato si no se necesita modificar render
      let _ = p.analyze_render_frame([&buf]); // o p.process_render_frame([&mut buf])
    }
    // Nota: lib.rs no expone reinitialize/get_stats en NoiseSuppressor hoy — añadir wrapper:
    // pub fn get_stats(&self) -> Option<webrtc_audio_processing::Stats> { self.processor.as_ref().map(|p| p.get_stats()) }
    // pub fn reinitialize(&self) { if let Some(p)=&self.processor { p.reinitialize(); } }
  }
  pub fn process(&mut self, frame: &[i16]) -> Vec<i16> {
    // ... como hoy: tmp f32 → p.process_capture_frame([&mut tmp]) → cast a i16
  }
}
```

### 8.4 `get_stats` + trend (copiado de `tests/aec_cancel.rs:227-249`)

```rust
fn suppression_trend_db(capture_echo: &[i16], far: &[i16], feed_render: bool) -> Vec<f64> {
  // por-second Goertzel 440 Hz — ver crates/lumen-voice/tests/aec_cancel.rs: suppression_trend
  // útil para ver si AEC converge en 0..1 s o nunca (línea plana -22 vs -25)
}
let st = processor.get_stats();
eprintln!("AEC stats delay_ms={:?} erl={:?} erle={:?} rel={:?} recent_max={:?}",
  st.delay_ms, st.echo_return_loss, st.echo_return_loss_enhancement,
  st.residual_echo_likelihood, st.residual_echo_likelihood_recent_max);
// reinitialize si echo_path_change detectado (delay_ms salta + erle colapsa)
processor.reinitialize();
```

### 8.5 Regenerar defaults.json5 localmente (para diff vs tuned)

```sh
cargo run -p webrtc-audio-processing --example aec_config --features serde,experimental-aec3-config -- dump > /tmp/defaults.json
cargo run -p webrtc-audio-processing --example aec_config --features serde,experimental-aec3-config -- multichannel-dump > /tmp/mc.json
```

---

## 9) Checklist PipeWire 48k mono en i5-4590

- [ ] `cpal` captura/preferred 48k mono (ya en `CaptureResampler`), `AudioOutput` push resample 48k→device y tap  device→48k stateful, phase-preserving (audit `drain_into` OK)
- [ ] Cada `process_*_frame` recibe exactamente 480 (no 960 batch); mic 960 se splitea 480+480
- [ ] Lockstep `RENDER_CAP 150 ms` (7200), `to_feed min(available,960)`, rms gate `>0.002`
- [ ] NS VeryHigh producción + harness secundario `NS None` como gate AEC real (≥ -12 dB expect)
- [ ] `get_stats` polling 5 s + per-second trend en CI
- [ ] `initial_state_seconds 2.5` + patch transparente activo (medir primeros 2 s sin clamp voz)
- [ ] `HPF apply_in_full_band=true` siempre con AEC
- [ ] No forzar `stream_delay_ms Some()` salvo medición HAL; dejar `None`
- [ ] En MSVC, no activar `experimental-aec3-config` (gate non-msvc como `FastEnhancer`)

---

## 10) Por qué no cambiar de stack aún

- `sonora`/`sonora-aec3` (pure Rust AEC3 M145) resuelve FFI/build pero no supera en calidad a AEC3 bien tuneado; es plan B si corr no sube con 7.2 (ver `local/aec-reference.md` §5 B1/B2). `speexdsp`/`aec-rs` es MDF lineal sin DTD/RES/drift — peor double-talk y Larsen.
- Discord/Krisp siguen `HPF→AEC3→DNN-NS→AGC` con `stream_delay` auto — misma arquitectura que lumen. La diferencia no es códec sino **medición**: Discord mide ERLE sobre linear AEC, no post-Krisp; lumen medía post-NS.

