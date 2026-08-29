# FE Windows — causa raíz "CPU no compatible" (FE/S/M en Windows vs Linux)

**Fecha:** 2026-08-26  
**Repo:** `discord-light` — `crates/lumen-voice/{build.rs,src/audio.rs}`, `vendor/faster-enhancer/*`, `apps/lumen-slint/{src/{voice,controller}.rs,ui/settings.slint}`, `.github/workflows/{ci,release}.yml`, `.cargo/config.toml`  
**CPU de referencia:** `Intel i5-4590 @3.30GHz` (Haswell, 4C/4T) — host de esta WS y del reporte original (`x86_64-unknown-linux-gnu`, `target_env=gnu` en Linux, `target_env=msvc` en Windows-MSVC). Flags verificados en `/proc/cpuinfo`: `sse4_1`, `avx2`, `fma`, `f16c` presentes.

---

## 1. Hipótesis principal — confirmada

### 1a. El stub `cfg(target_env="msvc")` en `audio.rs` (líneas exactas)

`crates/lumen-voice/src/audio.rs:1311-1485`:

```rust
// 1314
#[cfg(not(target_env = "msvc"))]
mod ffe { extern "C" { pub fn fe_init(...); pub fn fe_run(...); pub fn fe_free(); pub fn fe_s_init(...); ... } }

// 1362-1390  — rama real (non-msvc)
#[cfg(not(target_env = "msvc"))]
impl FastEnhancerDenoiser {
    pub fn available() -> bool {
        #[cfg(target_arch="x86_64")]
        { (is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") && is_x86_feature_detected!("f16c"))
          || is_x86_feature_detected!("sse4.1") }
        #[cfg(target_arch="aarch64")] { true }
        #[cfg(not(any(...)))] { false }
    }
    pub fn new() -> Option<Self> { Self::build(ffe::fe_init, ..., Self::WEIGHTS_M, 320) }
    pub fn new_small() -> Option<Self> { Self::build(ffe::fe_s_init, ..., Self::WEIGHTS_S, 512) }
    fn build(init, run, free, weights, frame_size) -> Option<Self> { if init(...)==0 {Some(...)} else {None} }
}

// 1469-1485 — rama stub (msvc) — SIEMPRE no disponible
// On MSVC the C runtime is not built; the type exists so the tier wiring compiles, but `new()` always returns None → NS-only chain.
#[cfg(target_env = "msvc")]
impl FastEnhancerDenoiser {
    pub fn new() -> Option<Self> { None }
    pub fn new_small() -> Option<Self> { None }
    pub fn available() -> bool { false }
    pub fn process(&mut self, frame: &[i16]) -> Vec<i16> { frame.to_vec() }
}
```

**Puntos clave:**
- `available()` en MSVC es `false` incondicional, sin `is_x86_feature_detected!` ni llamada al runtime.
- `new()` / `new_small()` devuelven `None` sin intentar `fe_init`, aunque `build.rs` haya compilado `libfe.a` con MinGW y el binario esté linkeado con `lld-link`.
- `SuppressorModel::available()` (`audio.rs:1292-1298`) delega a `FastEnhancerDenoiser::available()`, y es lo que la UI consume.

**Por qué Linux sí pasa con el mismo CPU:** en Linux `target_env` es `gnu`, se compila la rama `#[cfg(not(target_env="msvc"))]`. `available()` hace `is_x86_feature_detected!("sse4.1")` → `true` en i5-4590 (ver `flags` abajo), y `new()` llama `fe_init` que a su vez hace `fe_qgemm_init()` → `select_ops` instala `sse41` o `avx2` según `fe_cpu_x86_caps()` → `fe_init` retorna `0`. En Windows el mismo `is_x86_feature_detected!` devolvería `true`, pero **nunca se ejecuta** por el `cfg` — el compilador descarta esa rama por completo. Es un gate de **compile-time**, no de hardware.

Evidencia de la máquina actual (este Linux, mismo micro que el del reporte dual-boot):

```
$ grep -m1 "model name" /proc/cpuinfo
  Intel(R) Core(TM) i5-4590 CPU @ 3.30GHz
$ grep flags /proc/cpuinfo | tr ' ' '\n' | grep -E "sse4_1|avx2|fma|f16c"
  sse4_1
  avx2
  fma
  f16c
$ rustc --print cfg | grep target_env
  target_env="gnu"

# Si el mismo binario se compilara con --target x86_64-pc-windows-msvc:
#   target_env="msvc"  =>  available() == false  aunque el CPU sea idéntico
#   target_arch="x86_64" (igual), target_os="windows" vs "linux"
```

Conclusión: **hardware idéntico → resultado distinto por `cfg(target_env)`**.

---

## 2. Gate de `build.rs` en MSVC — el segundo bloqueo

`crates/lumen-voice/build.rs:206-258` (función `main`, rama MSVC):

```rust
if target_env.contains("msvc") {
    let flags = env::var("CARGO_ENCODED_RUSTFLAGS")
        .or_else(|_| env::var("RUSTFLAGS"))
        .unwrap_or_default();
    if !flags.contains("lld-link") {                // ← gate
        println!("cargo:warning=lumen-voice: faster-enhancer C runtime skipped on \
                  MSVC (link.exe can't read the MinGW .a — build with \
                  RUSTFLAGS=\"-C linker=lld-link\" to enable FastEnhancer). \
                  WebRTC NS-only tier in effect.");
        return;                                     // ← early return, no link fe
    }
    // cross-compile MinGW gcc + Ninja, produce GNU .a, lld-link lo lee junto a MSVC .lib
    let build_dir = out.join("fe-build-mingw");
    let st = cmake(&["-S", root, "-B", build_dir, "-G", "Ninja",
                     "-DCMAKE_C_COMPILER=gcc", "-DCMAKE_BUILD_TYPE=Release",
                     "-DFE_BUILD_TESTS=OFF", "-DFE_ENABLE_PROFILE=OFF"])
             .and_then(|_| cmake(&["--build", build_dir, "--target", "fe"]));
    match st {
        Ok(()) => {
            println!("cargo:rustc-link-search=native={}", build_dir.display());
            println!("cargo:rustc-link-lib=static=fe");
        }
        Err(e) => println!("cargo:warning=lumen-voice: faster-enhancer C runtime not built on MSVC ({e}); ..."),
    }
    return;   // ← BUG secundario: no llama a build_fe_small → Small nunca se compila en Windows
}
// rama non-msvc: cmake normal + build_fe_small(...) + link m en !windows
```

**Verificación local (this WS, Linux):** `target/release/build/lumen-voice-*/output` muestra build completo de `fe` y `fe_s` con `arch: x86_64`, `dispatch: runtime`, compilando `fe_engine.c`, `qgemm_{avx2,sse41,avxvnni,avx512vnni}.c`, `fft_{avx2,sse2,avx512}.c`, etc., y `cargo:rustc-link-search/link-lib` para ambos. En Windows sin `RUSTFLAGS` env var el `output` contendría el `cargo:warning=... skipped on MSVC` y **no** `link-lib=fe` (no haydry-run disponible en Linux cross; inspección estática confirma el `return`).

**Por qué el gate está roto incluso cuando `.cargo/config.toml` ya tiene `lld-link`:**

`.cargo/config.toml:11-30` para `target.x86_64-pc-windows-msvc`:

```toml
[target.x86_64-pc-windows-msvc]
rustflags = ["-C", "linker=lld-link", "-C", "link-arg=/FORCE:MULTIPLE", ... ]
```

Cargo inyecta esos `rustflags` internamente al invocar `rustc`; **no aparecen** en `RUSTFLAGS` ni en `CARGO_ENCODED_RUSTFLAGS` del entorno del `build.rs`. El gate solo mira esas dos env vars, así que en un `cargo build` local en Windows **siempre falla** → FE nunca se compila, aunque el linker real **sí** sea `lld-link` y el `.a` GNU sea linkable. Solo CI/release pasan porque fijan explícitamente:

- `ci.yml:110` — `echo "RUSTFLAGS=-C linker=lld-link ..." >> "$GITHUB_ENV"`
- `release.yml:102` — `RUSTFLAGS: -C linker=lld-link ...` (env del step)

Por eso el artefacto de release **sí** debería incluir FE-Medium (Medium solo; ver bug Small abajo) pero el build local del usuario **no**.

**Bug adicional en la rama MSVC:** el `return` tras compilar `fe` nunca llama a `build_fe_small` (definida en `build.rs:156-204`). `build_fe_small` copia el vendor, parcha el `CMakeLists.txt` (renombra `fe`→`fe_s`, inyecta `add_compile_definitions(fe_init=fe_s_init ...)` + todos los `fe_s_*` de `SYMS`, y `-include cfg-s/fe_config_medium.h`), y compila `fe_s`. En Windows ese paso se omite, así que incluso con `RUSTFLAGS=...lld-link` el binario tendría **FE-M pero no FE-S**; la UI ofrece ambos y la selección S degradaría a NS-only (via `new_small()→None` del stub) aunque el runtime podría correr.

---

## 3. Flags por archivo y dispatch runtime (CMake + `qgemm_dispatch.c` / `cpu_detect.h`)

### CMake `vendor/faster-enhancer/CMakeLists.txt`

- Guard `if(MSVC) FATAL_ERROR` (22-25) — fe rechaza `cl.exe`/`clang-cl` y exige GNU-driver o MinGW (documentado: "MinGW or Clang GNU-driver Windows").
- Detección `FE_ARCH` por `CMAKE_SYSTEM_PROCESSOR` → `x86_64` (29-30) o `arm64`.
- Baseline `FE_FLAGS_COMMON` sin `-march` (56-61).
- Per-file ISA tiers (`FE_ARCH==x86_64`, 156-193):
  - `qgemm_avx2.c` → `-mavx2 -mfma` (158)
  - `qgemm_sse41.c` → `-msse4.1` (160 / 189 con `FE_QGEMM_HAVE_SSE41=1`)
  - `fft_avx2.c` → `-mavx2 -mfma` (162)
  - `fft_sse2.c` → `-msse2` (164) + `FE_QGEMM_HAVE_SSE41=1` (192)
  - `qgemm_avxvnni.c` → `-mavx2 -mfma -mavxvnni` (166)
  - `qgemm_avx512vnni.c` + `fft_avx512.c` → `-mavx2 -mfma -mavxvnni -mavx512f -mavx512bw -mavx512vl -mavx512vnni` (168-169)
  - `FE_X86_AVX2_TUS` = `fe_engine.c`, `winograd/fe_winograd.c`, `qgemm_quant.c`, `nn/{fe_activations,fe_attention,fe_gru,fe_qgemm,fe_sgemm}.c`, `fe_stft.c`, `nn/fe_vec.c` → `-mavx2 -mfma -mf16c` (175-187). Nota del CMake (171-174): estos TUs se compilan a AVX2 para que `FE_QGEMM_HAVE_AVX2` y los paths fp16 queden habilitados; un fallback SSE4.1 requeriría `__attribute__((target("sse4.1")))` por función + dispatch, no solo cambiar flags.

**Conclusión MinGW cross:** `FE_ARCH` detecta `x86_64` correctamente bajo MinGW (el toolchain es GNU, `__x86_64__` definido), y cada TU recibe su `-mavx2/-msse4.1` → el `libfe.a` contiene tanto kernels AVX2 como SSE4.1. El detector (`cpu_x86.c`) se compila **sin** `-mavx2`, así que no hace SIGILL al sondear.

### `src/qgemm/cpu_detect.h` + `src/qgemm/x86/cpu_x86.c` + `qgemm_dispatch.c:103-181`

`cpu_detect.h:39-50` — tiers x86:
```c
FE_X86_HAS_AVX2       = 1<<0,   // avx2 + fma + f16c + OS_AVX
FE_X86_HAS_AVXVNNI    = 1<<1,
FE_X86_HAS_AVX512VNNI = 1<<2,
FE_X86_HAS_OS_AVX     = 1<<3,   // xgetbv YMM preservado
FE_X86_HAS_OS_AVX512  = 1<<4,
FE_X86_HAS_SSE41      = 1<<5,   // sse4.1 — NO requiere OSXSAVE (XMM siempre preservado)
```

`cpu_x86.c:35-90` sonda `CPUID leaf1 ECX:19 → SSE4.1` (sin `OSXSAVE`), `leaf1 ECX:27/28/12/29 → AVX/FMA/F16C/OSXSAVE`, `xgetbv` para `OS_AVX/OS_AVX512`, `leaf7: AVX2, AVX512F/BW/VL/VNNI, AVX-VNNI`, y combina `has_avx2 && has_fma && has_f16c → AVX2`, etc., enmascarando con `OS_AVX/OS_AVX512`.

`qgemm_dispatch.c:105-135` (`select_ops`) instala **SSE4.1 primero** y deja que AVX2 lo sobrescriba:
```c
if (caps & FE_X86_HAS_SSE41)  fill_ops_sse41(ops);
if ((caps & FE_X86_HAS_AVX2) && (caps & FE_X86_HAS_OS_AVX)) fill_ops_avx2(ops);
// avxvnni, avx512vnni encima...
```

`fe_qgemm_init()`: `select_ops → if(tier==NONE) { fprintf(... "required: ARM NEON or x86 SSE4.1 / AVX2+FMA3+F16C"); return -1; }`.

`src/fe_pipeline.c:71-102` (`fe_init`):
```c
int fe_init(blob,size){
  if(g_s) return 0;
  fe_set_denormal_flush();           // FTZ/DAZ
  if(fe_qgemm_init()!=0) return -1;  // ← floor SSE4.1
  if(fe_load_weights(...)!=0) return -1;
  fe_weights_finalize_for_tier(&g_w, fe_qgemm_ops.tier);
  g_s = fe_state_create(); ...
  for(128) fe_process_frame(warmup); fe_reset();
  return 0;
}
```
Si el host no tiene ni SSE4.1, `fe_init` retorna `≠0` → `FastEnhancerDenoiser::build` retorna `None` → `NoiseSuppressor::with_model_and_aec` degrada a NS-only. Si tiene SSE4.1 aunque no AVX2, `fe_init==0` y el runtime usa el path `sse41` (`qgemm_sse41.c`: `pmovsxbw`/`pmaddwd`, 4-wide, software fp16, ~½ throughput de AVX2 pero real-time: 3-6% de un core según `audio.rs:1338-1340`).

**Correspondencia con `available()`:** `audio.rs:1373-1381` (rama non-msvc) hace `(avx2 && fma && f16c) || sse4.1`, que **es exactamente el floor de `fe_qgemm_init`** (AVX2 full-speed o SSE4.1 fallback). El comentario de `audio.rs:1336-1340` lo dice: "On SSE4.1-only CPUs the runtime uses software fp16 and 4-wide GEMM kernels at ~half throughput". El dispatch real en `qgemm_dispatch.c` confirma que `available()` no miente.

**Implicación para el stub:** si `fe_init` falla por falta de AVX2 pero SSE4.1 existe, el runtime **sí** corre; el stub actual que siempre dice `available=false` ocultaría esa capacidad. Y si `fe_init` falla por falta de ambos, `new()` ya retorna `None` → NS-only, pero la UI debería mostrar "no compatible" (lo que `available()` del stub hace, pero por la razón equivocada).

---

## 4. Plumbing UI — trazado del mensaje exacto

Cadena completa (grep):

```
crates/lumen-voice/src/audio.rs:1292  pub fn available(&self)->bool { match self { NsOnly=>true, FastEnhancerS/M => FastEnhancerDenoiser::available() } }
crates/lumen-voice/src/audio.rs:1373  impl FastEnhancerDenoiser::available()  (non-msvc: is_x86_feature_detected!; msvc: false)
apps/lumen-slint/src/controller.rs:616  ui.set_voice_suppressor_model_available(lumen_voice::audio::FastEnhancerDenoiser::available());
apps/lumen-slint/src/voice.rs (similar wiring del VoiceController)
apps/lumen-slint/ui/app.slint:118  in-out property <bool> voice-suppressor-model-available;
apps/lumen-slint/ui/settings.slint:23  in property <bool> suppressor-model-available;
apps/lumen-slint/ui/settings.slint:153  current-index: root.suppressor-model == "fastenhancer-s" && root.suppressor-model-available ? 0 : (fastenhancer && available ?1:2)
apps/lumen-slint/ui/settings.slint:160-161  if (fastenhancer || fastenhancer-s) && !available : Text {
  text: "FastEnhancer no es compatible con este CPU (requiere SSE4.1 o superior). Se usará NS (WebRTC).";
  color: AppTheme.colors.rose;
}
```

**Mensaje buscado:** `"CPU no compatible"` del enunciado es la **paráfrasis** del tooltip real; el string literal en el repo es:

> **`"FastEnhancer no es compatible con este CPU (requiere SSE4.1 o superior). Se usará NS (WebRTC)."`**

Ubicación: `apps/lumen-slint/ui/settings.slint:161` (y el `ComboBox` queda degradado a NS-only: `current-index` fuerza `2` cuando `!available`).

También aparece en la lógica de `SuppressorModel::available()` y `FastEnhancerDenoiser::available()` como la condición que dispara ese `Text`.

---

## 5. Propuesta de fix seguro (sin tocar AEC)

### 5a. Principio

- **No usar `cfg(target_env="msvc")` para decidir disponibilidad.** El `target_env` distingue toolchain, no capacidad de CPU ni si el `.a` fue linkado.
- **Que `build.rs` emita un `cfg` cuando el build C tuvo éxito**, y que `audio.rs` haga `available()` / link del `mod ffe` en función de **ese `cfg`**, no del `target_env`. Así:
  - Si el MinGW build ok (sea cual sea el `target_env` del host), el Rust compila el FFI real y `available()` usa `is_x86_feature_detected!`.
  - Si el build se omitió (sin `lld-link` o sin `gcc`/`ninja`), el Rust compila un fallback que degrada a NS-only y `available()` refleja la **dispath real** (SSE4.1) en vez de `false` hardcodeado — pero `new()` no puede suceder porque no hay símbolos que linkar (evitar `undefined reference`).

### 5b. Cambios concretos (diff esbozo, sin romper AEC — `NoiseSuppressor`/`AudioOutput`/`render_tap` intactos)

#### `crates/lumen-voice/build.rs`

- Corregir el gate MSVC: no mirar solo `RUSTFLAGS` env, sino **emitir warning pero intentar el build si el linker real es `lld-link`** (que `.cargo/config.toml` ya fija). Dos opciones seguras:

  **Opción A (mínima, recomendada):** eliminar el gate por env var y **siempre intentar** el cross MinGW en MSVC; si falla (no `gcc`/`ninja`), warning + NS-only. Esto no requiere parsear `.cargo/config.toml`.

  **Opción B:** además de `RUSTFLAGS`, sondar `CARGO_CFG_TARGET_FEATURE` / `RUSTFLAGS` de cargo config vía `cargo:rustc-env` es imposible; la forma canónica es emitir `cargo:rustc-cfg=fe_built` **solo** cuando el cmake tuvo éxito y dejar que el Rust haga `#[cfg(fe_built)]` vs `#[cfg(not(fe_built))]`. El gate por env var deja de bloquear.

- Emitir `cfg`:
  ```rust
  // tras cmake --build success en la rama msvc:
  println!("cargo:rustc-cfg=feature=\"fe_built\"");
  // y si se compila fe_s también:
  println!("cargo:rustc-cfg=feature=\"fe_s_built\"");
  // en la rama non-msvc, emitir lo mismo tras cada build ok
  println!("cargo:rustc-cfg=feature=\"fe_built\"");
  println!("cargo:rustc-cfg=feature=\"fe_s_built\"");
  ```

- **Fix Small en Windows:** extraer la lógica de `build_fe_small` a una función que reciba un flag `is_msvc: bool` y, cuando `is_msvc`, pase `-G Ninja -DCMAKE_C_COMPILER=gcc` igual que el Medium. En `main()` rama msvc, tras el build de `fe`, invocar `build_fe_small_mingw(root, &out)` en vez de `return`.

Esbozo completo de `build.rs:206-286`:

```rust
fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("vendor/faster-enhancer");
    println!("cargo:rerun-if-changed={}", root.display());
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let is_msvc = target_env.contains("msvc");

    if is_msvc {
        // Ya no gate por RUSTFLAGS env — .cargo/config.toml ya fija lld-link.
        // Intentar MinGW; si no hay gcc/ninja → warning NS-only, cfg no emitido.
        let build_dir = out.join("fe-build-mingw");
        let st = cmake(&["-S", root.to_str().unwrap(), "-B", build_dir.to_str().unwrap(),
                         "-G", "Ninja", "-DCMAKE_C_COMPILER=gcc",
                         "-DCMAKE_BUILD_TYPE=Release", "-DFE_BUILD_TESTS=OFF", "-DFE_ENABLE_PROFILE=OFF"])
                 .and_then(|_| cmake(&["--build", build_dir.to_str().unwrap(), "--target", "fe"]));
        match st {
            Ok(()) => {
                println!("cargo:rustc-link-search=native={}", build_dir.display());
                println!("cargo:rustc-link-lib=static=fe");
                println!("cargo:rustc-cfg=feature=\"fe_built\"");
                // Segundo runtime
                if let Err(e) = try_build_fe_small_mingw(&root, &out) {
                    println!("cargo:warning=lumen-voice: fe_s cross build failed ({e}); S tier NS-only.");
                } else {
                    println!("cargo:rustc-cfg=feature=\"fe_s_built\"");
                }
            }
            Err(e) => {
                println!("cargo:warning=lumen-voice: faster-enhancer C runtime not built on MSVC ({e}); WebRTC NS-only tier.");
                println!("cargo:warning=hint: ensure MinGW gcc + ninja on PATH (windows-latest already has them) and that .cargo/config.toml sets linker=lld-link (already does) — no extra RUSTFLAGS env needed after this fix.");
            }
        }
        return;
    }

    // non-msvc (Linux/macOS/GNU Windows) — sin cambios salvo emitir cfg
    let build_dir = out.join("fe-build");
    cmake(&["-S", root.to_str().unwrap(), "-B", build_dir.to_str().unwrap(),
             "-DCMAKE_BUILD_TYPE=Release", "-DFE_BUILD_TESTS=OFF", "-DFE_ENABLE_PROFILE=OFF"]).unwrap_or_else(|e| panic!("{e}"));
    cmake(&["--build", build_dir.to_str().unwrap(), "--target", "fe", "--config", "Release"]).unwrap_or_else(|e| panic!("{e}"));
    println!("cargo:rustc-link-search=native={}", build_dir.display());
    println!("cargo:rustc-link-lib=static=fe");
    println!("cargo:rustc-cfg=feature=\"fe_built\"");
    build_fe_small(&PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()), &out);
    println!("cargo:rustc-cfg=feature=\"fe_s_built\"");
    if !env::var("CARGO_CFG_TARGET_OS").map(|o| o=="windows").unwrap_or(false) {
        println!("cargo:rustc-link-lib=m");
    }
}
```

`try_build_fe_small_mingw` es `build_fe_small` adaptado: mismo `SYMS` + `-include cfg-s/fe_config_medium.h`, renombrado `fe_s`, pero con `-G Ninja -DCMAKE_C_COMPILER=gcc` en `cmake` (hoy `build_fe_small` solo usa `-S/-B` sin toolchain; en Linux el `cc` por defecto ya es `gcc`, en Windows-MSVC debe forzarse).

- Hacer `cargo:rerun-if-env-changed=RUSTFLAGS` y `CARGO_ENCODED_RUSTFLAGS` si se quiere mantener compat con overrides manuales (opcional, inocuo).

#### `crates/lumen-voice/src/audio.rs` (solo `FastEnhancerDenoiser`, sin tocar `NoiseSuppressor`/`AudioOutput`)

Reemplazar **todo** el `cfg(target_env="msvc")` por `cfg(feature="fe_built")`:

```rust
/// Raw FFI — solo cuando el C runtime fue linkado (build.rs emitió fe_built)
#[cfg(feature = "fe_built")]
mod ffe {
    use std::os::raw::{c_int, c_void};
    extern "C" {
        pub fn fe_init(weights_blob: *const c_void, weights_size: c_int) -> c_int;
        pub fn fe_run(in_: *const f32, out: *mut f32);
        pub fn fe_free();
        #[cfg(feature = "fe_s_built")]
        pub fn fe_s_init(weights_blob: *const c_void, weights_size: c_int) -> c_int;
        #[cfg(feature = "fe_s_built")]
        pub fn fe_s_run(in_: *const f32, out: *mut f32);
        #[cfg(feature = "fe_s_built")]
        pub fn fe_s_free();
    }
}

pub struct FastEnhancerDenoiser { run: ..., free: ..., frame_size: usize, in_buf: Vec<f32>, out_buf: Vec<f32>, run_buf: Vec<f32>, denoise_buf: Vec<f32>, }

#[cfg(feature = "fe_built")]
impl FastEnhancerDenoiser {
    const WEIGHTS_M: &'static [u8] = include_bytes!("../vendor/faster-enhancer/weights/fe.q8");
    const WEIGHTS_S: &'static [u8] = include_bytes!("../vendor/faster-enhancer/weights/fe_s.q8");
    pub fn available() -> bool {
        #[cfg(target_arch="x86_64")]
        { (is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") && is_x86_feature_detected!("f16c"))
          || is_x86_feature_detected!("sse4.1") }
        #[cfg(target_arch="aarch64")] { true }
        #[cfg(not(any(target_arch="x86_64", target_arch="aarch64")))] { false }
    }
    pub fn new() -> Option<Self> { Self::build(ffe::fe_init, ffe::fe_run, ffe::fe_free, Self::WEIGHTS_M, 320) }
    #[cfg(feature="fe_s_built")]
    pub fn new_small() -> Option<Self> { Self::build(ffe::fe_s_init, ffe::fe_s_run, ffe::fe_s_free, Self::WEIGHTS_S, 512) }
    #[cfg(not(feature="fe_s_built"))]
    pub fn new_small() -> Option<Self> { None }
    fn build(...) -> Option<Self> { ... } // igual
    pub fn process(&mut self, frame: &[i16]) -> Vec<i16> { ... } // igual
}
#[cfg(feature="fe_built")]
impl Drop for FastEnhancerDenoiser { fn drop(&mut self){ unsafe{(self.free)()} } }

// Fallback sin runtime — sin símbolos C que linkar, pero available() refleja el dispatch real
// (no "false" hardcodeado que ocultaba el tier SSE4.1)
#[cfg(not(feature="fe_built"))]
impl FastEnhancerDenoiser {
    pub fn available() -> bool {
        #[cfg(target_arch="x86_64")]
        { (is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") && is_x86_feature_detected!("f16c"))
          || is_x86_feature_detected!("sse4.1") }
        #[cfg(target_arch="aarch64")] { true }
        #[cfg(not(any(target_arch="x86_64", target_arch="aarch64")))] { false }
    }
    pub fn new() -> Option<Self> { None }
    pub fn new_small() -> Option<Self> { None }
    pub fn process(&mut self, frame: &[i16]) -> Vec<i16> { frame.to_vec() }
}
```

**Variante si no se quierefeature gating:** usar `#[cfg(fe_built)]` sin `feature=` (i.e. `println!("cargo:rustc-cfg=fe_built")` y `#[cfg(fe_built)]`). Ambas funcionan; `feature=` es más idiomático y aparece en `cargo --print cfg`.

**Invariantes que se preservan:**
- APIs estables: `SuppressorModel::available()`, `as_str/parse`, `NoiseSuppressor::with_model_and_aec`, `VoiceClient::set_aec_enabled/aec_enabled` sin cambios.
- `NoiseSuppressor::with_model_and_aec` ya degrada a NS-only si `FastEnhancerDenoiser::new()` es `None`; el contrato no cambia.
- AEC intacto: no se toca `NoiseSuppressor`/`AudioOutput`/`render_tap` (rango 620-1250 respetado).

#### Alternativa sin `lld-link` (build NS-only)

Documentar en `build.rs` warning y en este doc: si el usuario **realmente** compila con `link.exe` (sin `lld-link`), el `.a` MinGW no es legible (`link.exe can't read the MinGW .a`). El fallback NS-only es correcto; el warning debe decir:

> `lumen-voice: faster-enhancer C runtime skipped (link.exe can't read MinGW .a — set RUSTFLAGS="-C linker=lld-link" or rely on .cargo/config.toml's [target.x86_64-pc-windows-msvc] rustflags; on this repo lld-link is already the default, no manual RUSTFLAGS needed)`

Tras el fix de eliminar el gate por env var, este caso solo ocurre si el usuario borró `.cargo/config.toml` o fuerza `RUSTFLAGS=-C linker=link.exe`.

#### Toolchain

No requiere toolchain distinto: **MinGW gcc + Ninja + lld-link ya están en CI** (`windows-latest` trae `gcc` MinGW en PATH; `rustup component add llvm-tools-preview` provee `lld-link` via `rust-lld`; `.cargo/config.toml` fija `linker=lld-link`). `build.rs` ya usa `-G Ninja -DCMAKE_C_COMPILER=gcc`. Localmente el usuario de Windows solo necesita tener MinGW `gcc` en `PATH` (ya está en `windows-latest` y en `msys2`/`Git for Windows`); si no lo tiene, el build degrada a NS-only con warning — nunca hard failure.

---

## 6. CI / release — por qué el artefacto debería incluir FE pero el build local no

| Workflow | Línea RUSTFLAGS | Efecto |
|---|---|---|
| `.github/workflows/ci.yml:97-110` | `echo "RUSTFLAGS=-C linker=lld-link ... -C link-arg=/FORCE:MULTIPLE ..." >> "$GITHUB_ENV"` en `desktop (windows-latest)` | `CARGO_ENCODED_RUSTFLAGS` contiene `lld-link` → `build.rs` supera el gate → compila `libfe.a` MinGW (Medium) → link con `lld-link` junto a `webrtc-audio-processing.lib`. El bin `target/debug/lumen.exe` incluye FE-M (pero **no** FE-S por el bug del `return`). |
| `.github/workflows/release.yml:94-103` (`build-windows`) | `RUSTFLAGS: -C linker=lld-link ...` en env del `cargo build --release` | Igual que CI, pero `--release`. Artefacto `lumen-windows/lumen.exe` con FE-M. |

**Por qué el usuario local ve "no compatible":** su `cargo build` en `x86_64-pc-windows-msvc` sí linka con `lld-link` gracias a `.cargo/config.toml:13` (`"-C","linker=lld-link"`), **pero** `build.rs` mira solo `RUSTFLAGS`/`CARGO_ENCODED_RUSTFLAGS` del env (no el `.cargo/config.toml`). Como el usuario no exportó `RUSTFLAGS` manualmente, el gate falla → `return` temprano → no `cargo:rustc-link-lib=static=fe` → el Rust compila el stub `available=false` → UI dice no compatible, aunque el hardware (mismo i5-4590) sí tiene SSE4.1/AVX2 y `fe_qgemm_init` habría retornado `0`.

Tras el fix propuesto, el build local usará `.cargo/config.toml` y el `cfg` emitido por el success del cmake, así que **local == CI**.

---

## 7. Checklist de validación (sin formatters ni suite completa — contract)

- [x] Lectura de `audio.rs:1311-1485` (stub vs real) y `build.rs:219-258` (gate) — causa confirmada.
- [x] `is_x86_feature_detected!("sse4.1")` en i5-4590 vía `/proc/cpuinfo` → `available` true en Linux.
- [x] `CMakeLists.txt` per-file flags y `FE_ARCH==x86_64` con `-mavx2 -mfma -mf16c` / `-msse4.1` — cross MinGW produce ambos tiers.
- [x] `qgemm_dispatch.c` + `cpu_detect.h` muestran floor SSE4.1 y `fe_init` → `fe_qgemm_init` → `select_ops` con fallback — `available()` del stub actual es demasiado pesimista.
- [x] `grep` de `"FastEnhancer no es compatible con este CPU"` trazado a `settings.slint:161`, wiring `controller.rs:616` → `FastEnhancerDenoiser::available()`.
- [x] `ci.yml:110` y `release.yml:102` verificados con `lld-link`.
- [ ] (main se encarga) `cargo check -p lumen-voice --message-format=short` scoped — no correr `cargo fmt` / `cargo test` completo desde este slice (peers editan concurrentemente).
- [ ] Probar el parche: `cargo build -p lumen-voice` en Windows con y sin env `RUSTFLAGS` (con `.cargo/config.toml`), y `cargo build --target x86_64-pc-windows-msvc` en Linux cross (si se tiene `xwin`/`cargo-xwin`).

---

## 8. Archivos y líneas exactas (referencia rápida)

- `crates/lumen-voice/src/audio.rs:1311-1329` — `mod ffe` FFI (solo non-msvc)
- `crates/lumen-voice/src/audio.rs:1362-1467` — `FastEnhancerDenoiser` real (non-msvc, `available` con `is_x86_feature_detected!`)
- `crates/lumen-voice/src/audio.rs:1471-1485` — `FastEnhancerDenoiser` stub (msvc, `available=false`)
- `crates/lumen-voice/src/audio.rs:1292-1298` — `SuppressorModel::available()` → `FastEnhancerDenoiser::available()`
- `crates/lumen-voice/src/audio.rs:1006-1015` — `NoiseSuppressor::with_model_and_aec` (usa `new()`/`new_small()`)
- `crates/lumen-voice/build.rs:13-20` — comentario lld-link + MinGW
- `crates/lumen-voice/build.rs:48-155` — `SYMS` (prefijo `fe_s_`)
- `crates/lumen-voice/build.rs:156-204` — `build_fe_small` (no invocado en MSVC)
- `crates/lumen-voice/build.rs:219-258` — gate `!flags.contains("lld-link")` → `return` (causa)
- `crates/lumen-voice/vendor/faster-enhancer/CMakeLists.txt:20-25` — `if(MSVC) FATAL_ERROR`
- `crates/lumen-voice/vendor/faster-enhancer/CMakeLists.txt:26-33` — `FE_ARCH` detect
- `crates/lumen-voice/vendor/faster-enhancer/CMakeLists.txt:156-193` — per-file `-mavx2/-msse4.1/-mf16c`
- `crates/lumen-voice/vendor/faster-enhancer/src/qgemm/cpu_detect.h:39-46` — `FE_X86_HAS_SSE41` / `FE_X86_HAS_AVX2`
- `crates/lumen-voice/vendor/faster-enhancer/src/qgemm/x86/cpu_x86.c:35-89` — `fe_cpu_x86_caps()` (CPUID + xgetbv)
- `crates/lumen-voice/vendor/faster-enhancer/src/qgemm/qgemm_dispatch.c:105-181` — `select_ops` / `fe_qgemm_init` (SSE4.1 floor)
- `crates/lumen-voice/vendor/faster-enhancer/src/qgemm/arch_kernels.h:179-192` — `qgemm_sse41_*` prototypes
- `crates/lumen-voice/vendor/faster-enhancer/src/fe_pipeline.c:71-81` — `fe_init → fe_qgemm_init` (return -1 si no SSE4.1)
- `apps/lumen-slint/src/controller.rs:616` — `set_voice_suppressor_model_available(FastEnhancerDenoiser::available())`
- `apps/lumen-slint/ui/settings.slint:151-166` — `SuppressorModel` ComboBox + `available` → `forces NS-only` + mensaje `FastEnhancer no es compatible...`
- `apps/lumen-slint/ui/app.slint:118` — `voice-suppressor-model-available` prop
- `.cargo/config.toml:11-30` — `[target.x86_64-pc-windows-msvc] rustflags linker=lld-link` (ya fija lld-link sin env)
- `.github/workflows/ci.yml:97-110` — `RUSTFLAGS=-C linker=lld-link ...` (CI Windows)
- `.github/workflows/release.yml:97-103` — `RUSTFLAGS: -C linker=lld-link ...` (release Windows)
- `crates/fe-ab/build.rs:328-336` — referencia de symbol prefixing `-D{n}={sym}_{n}`

---

## 9. Notas de seguridad / alternativas descartadas

- **No cambiar a `target_os="windows"` como gate:** `x86_64-pc-windows-gnu` (MinGW) sí puede compilar FE nativo y no necesita `lld-link`; el gate debe ser por toolchain de link, no por OS.
- **No forzar `available()` a `true` en MSVC:** el runtime puede realmente no estar linkado (compilación sin gcc); `available()` debe seguir siendo fiable. Con `cfg(fe_built)` se cumple.
- **No tocar `crates/fe-ab`:** es crate de benchmark/para AB tests, no afecta `lumen-voice` release; se cita solo como referencia de prefixing.
- **No tocar AEC:** `NoiseSuppressor`/`AudioOutput` fuera de alcance (Slice FE no solapa con 620-1250).
