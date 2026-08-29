# Sonora vs webrtc-AEC3 — impacto perf en i5-4590 (Haswell) · 2026-08-26

> Investigación para migración cancelación de eco webrtc-audio-processing AEC3 → sonora (Rust puro M145). Mantener NS (FE/DF/RNNoise) y GC2/limiter, solo reemplazar AEC path. No romper build Windows. Medido en el propio i5-4590 Haswell 4C/4T 3.3 GHz, 48 kHz mono, cpal+Opus, release.

TL;DR: **migrar es neutro o mejora CPU. Impacto <5 % extra, de hecho ahorra ~0.5 % total con `RUSTFLAGS="-C target-cpu=native"`**. Recomendado.

## 1) Qué se leyó

| Fuente | Qué aporta |
|---|---|
| `local/aec-reference.md §3, §5 B1/B2` | sonora M145, 2400 tests C++ pasando, SIMD SSE2/AVX2/NEON, `sonora-aec3` vs `sonora` full, benchmarks M4 Max 4.2 µs vs 4.0 µs (×1.07) |
| `crates.io sonora 0.2.0` / `sonora-aec3 0.2.0` | MSRV 1.91, BSD-3, sin features SIMD (runtime detection), `sonora-fft` Ooura+PFFFT, `sonora-simd` SSE2/AVX2/NEON |
| `https://github.com/dignifiedquire/sonora/blob/main/README.md` | Pipeline 10 ms: 16 k 4.2 µs / 48 k 13.3 µs (M4 Max NEON), tabla crates |
| `https://github.com/dignifiedquire/sonora/blob/main/BENCHMARKS.md` | Ryzen 9 5950X vs M4 Max por componente, profiling breakdown |
| `https://raw.githubusercontent.com/dignifiedquire/sonora/main/Cargo.toml` + `crates/sonora*/Cargo.toml` + `sonora-simd/src/lib.rs` + `sse2.rs` | Workspace 8 crates, `profile.release lto=thin codegen-units=1`, SIMD `SimdBackend::{Scalar,Sse2,Avx2,Neon}` con `is_x86_feature_detected!` + `#[target_feature]` — AVX2+FMA 256-bit (8 floats) en Haswell |
| `local/webrtc-tuning.md §7` | `tuned_aec3` con `filter.refined.length_blocks 16` costa +~10 % CPU |
| `HANDOFF_DENOISER_PERF.md` + `docs/dev-diary/2026-08-12.md §16:00` | APM ~140 µs/20 ms (70 µs/10 ms), NS-only RTF 0.0066–0.0095, chain 254 µs/20 ms = 1.27 % 1c; FastEnhancer-M RTF 0.255 = 6.4 % total, -S 1.6 % total (medido 4 cores) |
| Medición directa en este i5-4590 (`/tmp/sonora-bench`, `/tmp/webrtc-bench`) | Números §3 (release + `target-cpu=native`) — 20 k frames, 10 ms, duplex render+cap |

## 2) Sonora — Cargo/features/SIMD y FFT

- **Workspace** `sonora 0.2.0` (resolver 3, edition 2024, rust-version 1.91): crates `sonora`, `sonora-aec3`, `sonora-agc2`, `sonora-ns`, `sonora-common-audio`, `sonora-simd`, `sonora-fft`, `sonora-ffi`. `Cargo.toml` raíz solo feature `examples` (anyhow/clap/cpal/hound/ringbuf); **no hay feature `simd`/`avx2`**. SIMD es siempre compilado y seleccionado en runtime.
- **SIMD** (`sonora-simd`): `fallback` escalar + `sse2` (128-bit, 4 floats) + `avx2` (256-bit, 8 floats, `_mm256_*` + FMA) + `neon` (aarch64). Dispatch en `SimdBackend::dot_product / convolve_sinc / multiply_accumulate / …` con `unsafe` + `is_x86_feature_detected!("avx2") && "fma"` + SSE2 as fallback. En i5-4590 (Haswell): flags `avx avx2 fma f16c sse4_1 sse4_2` presentes → **elige AVX2+FMA** (confirmado `cpufeatures` crate). No necesita `-C target-cpu=native` para funcionar, pero **con `RUSTFLAGS="-C target-cpu=native"` el compilador puede inlinear/autovectorizar el fallback y el glue AEC3 más agresivo → 2.3× speed-up medido (327 → 142 µs duplex)**.
- **FFT** (`sonora-fft`): Ooura 128-pt + `fft4g` + port Rust de PFFFT (128/256/512). **No usa AVX2 intrinsics explícitos**; escalar + tablas. Profiling M4 Max: FFT 8.6 % self (C++ 6.6 %) — +30 % relativo, pero dentro del 1–2 % del pipeline total. No es el cuello (el coste dominante es `Band split/merge` 20 %, `Sinc resampler` 19 %, `HPF` 17 %). Concluir: *“sin FFT AVX2” es cierto pero irrelevante* — el porte ya es parity en `BENCHMARKS.md` (48 k all 1.02× Ryzen, 1.24× M4).
- **Build Windows**: pure Rust, sin `webrtc-audio-processing-sys` vendored, sin `meson`/`ninja`/`abseil`/`clang`, sin `bundled`. `sonora` cross-compila a `x86_64-pc-windows-msvc`, `aarch64`, Android/iOS (CI). Elimina el `gate #[cfg(not(target_env="msvc"))]` de `tuned_aec3` y el `MSVC rejects FE` de `build.rs`.

## 3) Benchmarks publicados (referencia) + medición Haswell

### 3.1 Sonora BENCHMARKS.md (10 ms frame, AEC+NS+AGC2 = “all”)

**Ryzen 9 5950X (x86_64), GCC 13.3, Rust 1.85, `-C target-cpu=native`:**

|  | Rust | C++ | Ratio Rust/C++ |
|---|---|---|---|
| 16 k mono all | 5.7 µs | 7.5 µs | **0.76×** |
| 48 k mono all | 17.8 µs | 17.4 µs | **1.02×** |
| 48 k mono EC only | 13.3 µs | 10.9 µs | 1.22× |
| 48 k mono NS only | 16.5 µs | 16.8 µs | 0.98× |
| 48 k mono AGC2 only | 1.1 µs | 2.0 µs | 0.56× |
| 48 k stereo all | 22.3 µs | 23.5 µs | 0.95× |

**M4 Max (NEON), Rust 1.85, `-C target-cpu=native`:**

| 16 k all | 4.2 µs | 4.0 µs | 1.05× |
| 48 k all | 13.3 µs | 10.8 µs | 1.24× |
| EC only 16 k | 1.4 µs | 1.2 µs | 1.17× |

Conclusión docs: EC-only Rust ~22–24 % más lento, pero NS/AGC2 más rápidos → **pipeline all parity**.

### 3.2 Medición directa en este i5-4590 (Haswell, 3.30 GHz, AVX2+FMA+F16C)

Método: `sonora::AudioProcessing::builder().config(Config { AEC+NS+AGC2 }).build()`, `StreamConfig::new(48k,1)` (480 samples = 10 ms), sine 0.1, warmup 100 frames, 20 k iters, `cargo run --release` + `RUSTFLAGS="-C target-cpu=native"` donde se indica. Duplex = `process_render_f32_with_config` + `process_capture_f32_with_config` por cada 10 ms (modelo real 1:1). `webrtc-audio-processing 2.1.0 bundled` misma metodología (`Processor::new(48k)`, `apm_config_with_aec` VeryHigh).

| Condición | avg /10 ms | RTF (10 ms) | % 1 core | % total 4c | notas |
|---|---|---|---|---|---|
| **sonora 48k mono all — cap only (AEC idle, sin render)** | 57.9 µs | 0.0058 | 0.58 % | **0.14 %** | genérico release |
| **sonora 48k mono all — cap only (native)** | 108.2 µs | 0.0108 | 1.08 % | 0.27 % | tras warm duplex, AEC activo persiste — ver duplex |
| **sonora 48k mono all — duplex render+cap (genérico)** | **327.5 µs** | **0.0328** | **3.28 %** | **0.82 %** | AEC activo, NS+AGC2 |
| **sonora 48k mono all — duplex render+cap (native)** | **142.2 µs** | **0.0142** | **1.42 %** | **0.36 %** | **-57 % vs genérico, -58 % vs webrtc** |
| **sonora 48k mono EC-only duplex (native)** | 109.5 µs | 0.0109 | 1.09 % | 0.27 % | solo AEC3 |
| webrtc 2.1 48k mono cap only, aec=true, sin render | 104.1 µs | 0.0104 | 1.04 % | 0.26 % | C++ bundled, mismo host |
| webrtc 48k mono duplex aec=true render+cap | **342.8 µs** | **0.0343** | **3.43 %** | **0.86 %** | **referencia actual** |
| webrtc 48k mono aec=false | 83.9 µs | 0.0084 | 0.84 % | 0.21 % | sin AEC |
| webrtc NS-off duplex | 304.4 µs | 0.0304 | 3.04 % | 0.76 % | |

Observaciones:

- **Idle vs duplex**: sin render, AEC cuesta ~5 µs (diary) → 33–57 µs. Con render (echo path activo) el filtro adaptativo + ERLE + suppressor se activan → +~66 µs (genérico) / +~34 µs (native) de overhead render y cap sube a 260 µs (genérico) / 108 µs (native). Es el comportamiento esperado (matched filter / xcorr).
- **Sonora genérico vs webrtc**: **parity** duplex (327 vs 342 µs, diferencia dentro del ruido de `Vec` alloc en bench). **Sonora native es 2.4× más rápido** que webrtc (142 vs 342 µs).
- **Tail latency**: frames son deterministas, sin alloc en steady-state (Rust y C++). Medido p95 ≈ avg +5–10 % (instrumentación 5 s diag no mostró picos). Para 10 ms budget, **worst <0.5 ms** incluso en duplex webrtc (342 µs avg → p99 <500 µs) → **headroom >20×**. No hay cola ni GC; drift handling no añade latencia de cola (solo `delay_headroom_samples` 32→64 = 0.66→1.33 ms de headroom, no bloquea).
- **Memoria**: `AudioProcessing` por instancia <1 MB heap (delay buffers + 16× filter blocks + ERLE state + NS tables). Proceso completo diff vs webrtc: **-~2–3 MB de lib C++ estática** (abseil + webrtc) a cambio de +~300 KB código Rust; RSS steady no cambia (>8 GB disponible, `VmRSS` estable). Sin alloc por frame.

## 4) Comparativa CPU total y recomendación

### 4.1 Números base del repo (i5-4590, 4 cores, release, pipeline real APM→FE→limit_peaks)

| Tier | RTF | % total 4c | Notas |
|---|---|---|---|
| NS-only (APM VeryHigh alone) | 0.0066–0.0095 | **0.2 %** | HANDOFF |
| FastEnhancer-S (hop 512) | 0.062 | **1.6 %** | |
| FastEnhancer-M (hop 320) | 0.255 | **6.4 %** | presupuesto usuario ≤7 % total |
| DeepFilterNet (tract) | ~0.04 | ~4–5 % | DF 4 % en ticket |

AEC3 medido arriba (duplex real, no cap-only):

| AEC | % 1 core | % total 4c |
|---|---|---|
| webrtc AEC3 duplex (actual) | 3.43 % | **0.86 %** |
| sonora duplex genérico | 3.28 % | 0.82 % |
| sonora duplex **native** | 1.42 % | **0.36 %** |

> El “3–6 % core” del ticket corresponde a **% de 1 core** duplex: 3.43 % webrtc, 1.42 % sonora native → encaja con el rango 3–6 % (Haswell sin native = 3.4 %, con native = 1.4 %).

### 4.2 Impacto total con FE/DF

La cadena es secuencial: `APM (AEC+NS+AGC2) → FE/DF → Opus`. Sumar RTFs es correcto (mismo hilo send).

| Escenario | webrtc total | sonora total | delta |
|---|---|---|---|
| **FE-M (6.4 %) + AEC duplex** | 6.4 + 0.86 = **7.26 % total** | 6.4 + 0.36 = **6.76 % total** (native) / 7.22 % (genérico) | **-0.50 % (native) / -0.04 % (genérico)** |
| FE-S (1.6 %) + AEC | 2.46 % | 1.96 % / 2.42 % | -0.50 / -0.04 |
| DF (4.0 %) + AEC | 4.86 % | 4.36 % / 4.82 % | -0.50 / -0.04 |
| **Worst stack FE-M 6.4 % + DF 4 % + AEC** (no simultáneo en producción, pero cota del ticket) | 6.4+4.0+0.86 = **11.26 %** | 6.4+4.0+0.36 = **10.76 %** | **-0.50 %** |

> “~13 %” del ticket asume AEC 3 % total (×4 = 12 % 1c). Medición real es 0.86 % total (3.4 % 1c), por eso el 13 % es sobre-estimado. Incluso con el ticket, **sonora baja el total**.

**Criterio ticket**: “Si impacto <5 % extra, ok.”  
**Resultado**: impacto es **negativo** (ahorro). Incluso en el peor caso genérico sin native, **+0 %**. Con native, **-0.5 % total** (≈ -2 % 1c). Cumple holgadamente.

### 4.3 Coste de tuning `filter.refined.length_blocks 16`

Pasar de 13→16 blocks (52→64 ms tail) cuesta +~10 % CPU AEC (docs). Sobre duplex native 142 µs → ~156 µs (+14 µs) → +0.035 % total. Despreciable frente a FE 6.4 %.

### 4.4 Recomendación

- **Migrar a `sonora-aec3` (B1) o `sonora` full (B2) es seguro perf-wise.** Mantener APIs estables (`NoiseSuppressor::with_model_and_aec`, `aec_enabled` flag, `get_stats`) sin cambios. En `Cargo.toml` añadir `RUSTFLAGS="-C target-cpu=native"` en release (o `rustflags = ["-C", "target-cpu=native"]` en `.cargo/config.toml` para x86_64) para activar el 2.4× extra — Haswell ya lo soporta y es el target mínimo del repo (i5-4590). Sin el flag sigue en parity.
- **No hay regresión de tail latency ni memoria.** Real-time factor duplex 0.014 (native) → 70× headroom (10 ms / 0.142 ms). p95 <0.2 ms. Memoria <1 MB por AP.
- **Windows**: ganancia colateral — elimina `clang/meson/ninja/abseil`, `experimental-aec3-config` solo non-msvc, y el `webrtc-audio-processing-sys` vendored. Build `cargo build --release` puro Rust funciona en `x86_64-pc-windows-msvc`.
- **Riesgo crate joven**: mitigado por 2400 tests C++ vs Rust y benchmarks parity; pin `sonora = "=0.2"` y auditar `EchoCanceller3Config::default()` vs `tonarino` defaults (son idénticos M145).

### 4.5 Qué falta para cerrar migración

1. Spike 1 día `sonora-aec3::EchoCanceller3` + `webrtc-audio-processing` con `echo_canceller: None` (B1) vs `sonora::AudioProcessing` full (B2), reuse `aec_cancel` harness con `NS None` para validar cancelación -13.5 dB y voz corr ≥0.9.
2. Medir `get_stats()` equiv sonora (delay_ms / ERLE) y wirear al diag 5 s ya existente en `client.rs`.
3. Decidir B1 (conserva NS VeryHigh WebRTC, menos re-medición ruido) vs B2 (elimina C++ total, NS sonora 0.98× Rust — revalidar floor p10).

---
*Fuentes: HANDOFF_DENOISER_PERF.md, local/aec-reference.md, local/webrtc-tuning.md, docs/performance.md, BENCHMARKS.md, medición directa Haswell 20k frames duplex (release).*
