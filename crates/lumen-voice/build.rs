//! Builds the vendored `faster-enhancer` C runtime (FastEnhancer-Medium, 48 kHz
//! W8A8 int8 denoiser) via its upstream CMake, then links `libfe` into
//! lumen-voice. See vendor/faster-enhancer/ for the source and its license.
//!
//! A second build (FastEnhancer-Small, 48 kHz, hop 512, C1=64/C2=48/F2=48)
//! is compiled with symbol prefixing (fe_s_*) so both runtimes coexist in
//! one binary — the UI offers M ("Ultra") and S ("Ligera") as selectable
//! tiers. The Small config is forced via `-include cfg-s/fe_config_medium.h`
//! (same include guard as the vendored config, so the vendored Medium
//! constants are skipped); the generated CMakeLists is the vendored one with
//! the target renamed and the prefix definitions injected.
//!
//! fe requires GCC/Clang-style per-file ISA flags and explicitly rejects
//! MSVC/clang-cl. On Windows-MSVC targets it is cross-compiled with MinGW gcc
//! (fe's own CMake documents "MinGW or Clang GNU-driver Windows" as supported;
//! gcc accepts `-mavx2` and produces a COFF GNU archive) and linked via
//! lld-link — the linker is set in `.cargo/config.toml` (`linker=lld-link`),
//! which reads both the MSVC .lib files (webrtc-audio-processing, which
//! requires MSVC) and the GNU .a archive (fe). No RUSTFLAGS env var is
//! required. If gcc/ninja are unavailable the build degrades to the WebRTC
//! NS-only tier with a warning — never a hard failure.
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn cmake(args: &[&str]) -> Result<(), String> {
    let st = Command::new("cmake").args(args).status();
    match st {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("cmake {:?} exited with {s}", &args[..2.min(args.len())])),
        Err(e) => Err(format!("failed to run cmake {:?}: {e}", &args[..2.min(args.len())])),
    }
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for e in fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let o = dst.join(e.file_name());
        if e.path().is_dir() {
            copy_dir(&e.path(), &o);
        } else {
            fs::copy(e.path(), &o).unwrap();
        }
    }
}

/// Global symbols defined by the runtime (from `nm --defined-only`), minus
/// the already-renamed public API (fe_init/fe_run/fe_free/fe_reset, handled
/// separately). The Small build prefixes every one with `fe_s_` so both
/// runtimes link into one binary; references inside each lib rename
/// consistently and libc calls are untouched. Same list as crates/fe-ab.
const SYMS: &[&str] = &[
    "fe_conv1d_k1_FCin",
    "fe_conv1d_k1_silu_concat2_fp16b",
    "fe_conv1d_k3_buf_silu",
    "fe_conv1d_k3_buf_silu_skip_fp16",
    "fe_conv1d_k3_winograd_silu",
    "fe_conv1d_k3_winograd_silu_skip_fp16",
    "fe_conv_transpose1d",
    "fe_cpu_brand",
    "fe_cpu_x86_caps",
    "fe_fft_forward_avx2",
    "fe_fft_forward_sse2",
    "fe_fft_forward_avx512",
    "fe_fft_init",
    "fe_fft_inverse_avx2",
    "fe_fft_inverse_sse2",
    "fe_fft_inverse_avx512",
    "fe_fft_plan_init",
    "fe_free_weights",
    "fe_gru_r_band",
    "fe_gru_step_fp16h",
    "fe_gru_z_band",
    "fe_irfft",
    "fe_istft",
    "fe_load_weights",
    "fe_mhsa",
    "fe_now_us",
    "fe_pack_W",
    "fe_process_frame",
    "fe_profile_dump",
    "fe_profile_record",
    "fe_profile_reset",
    "fe_qgemm_build_bias_eff_nobias",
    "fe_qgemm_compute_row_sums",
    "fe_qgemm_force_tier",
    "fe_qgemm_i8mm_repack_block",
    "fe_qgemm_init",
    "fe_qgemm_ops",
    "fe_qgemm_packed_calib_acc",
    "fe_qgemm_packed_calib_to_int8out",
    "fe_qgemm_packed_calib_transposed_in",
    "fe_qgemm_packed_calib_transposed_in_fp16",
    "fe_qgemm_packed_silu_calib_concat2_fp16b",
    "fe_qgemm_pack_W",
    "fe_qgemm_prequant",
    "fe_qgemm_repack_i8mm",
    "fe_qgemm_tail_unsupported",
    "fe_qg_x86_avx512",
    "fe_quantize_activation",
    "fe_quantize_activation_fp16",
    "fe_quantize_activation_transposed",
    "fe_quantize_activation_transposed_fp16",
    "fe_quantize_activation_with_scale",
    "fe_rfft",
    "fe_sgemm_packed",
    "fe_sgemm_packed_bias",
    "fe_silu_skip_fp16",
    "fe_softmax_rows_quant_from_int32",
    "fe_state_create",
    "fe_state_destroy",
    "fe_stft",
    "fe_stft_init",
    "fe_strided_conv1d",
    "fe_vec_add",
    "fe_weights_finalize_for_tier",
    "fe_winograd_f23_derive_weights",
    "fe_winograd_set_scratch",
    "fft_avx2_radix4_stage_wide",
    "fft_sse2_radix4_stage_wide",
    "fft_x86_radix4_stage",
    "g_fft_plan",
    "qgemm_avx2_fp32_fused",
    "qgemm_avx2_fp32_fused_acc",
    "qgemm_avx2_fp32_fused_acc_simd",
    "qgemm_avx2_fp32_fused_simd",
    "qgemm_avx2_fp32_fused_track_maxabs",
    "qgemm_avx2_fp32_fused_track_maxabs_simd",
    "qgemm_avx2_gru_full_fused_fp16inout",
    "qgemm_avx2_gru_full_fused_fp16inout_simd",
    "qgemm_avx2_int32",
    "qgemm_avx2_int32_k20",
    "qgemm_avx2_int32_simd",
    "qgemm_avx2_prefault_buffers",
    "qgemm_sse41_fp32_fused",
    "qgemm_sse41_fp32_fused_acc",
    "qgemm_sse41_fp32_fused_track_maxabs",
    "qgemm_sse41_gru_full_fused_fp16inout",
    "qgemm_sse41_gru_full_fused_fp16inout_full",
    "qgemm_sse41_int32",
    "qgemm_sse41_prefault_buffers",
    "qgemm_avx512vnni_fp32_fused",
    "qgemm_avx512vnni_fp32_fused_acc",
    "qgemm_avx512vnni_fp32_fused_track_maxabs",
    "qgemm_avx512vnni_gru_full_fused_fp16inout",
    "qgemm_avx512vnni_int32",
    "qgemm_avx512vnni_int32_k20",
    "qgemm_avxvnni_fp32_fused",
    "qgemm_avxvnni_fp32_fused_acc",
    "qgemm_avxvnni_fp32_fused_track_maxabs",
    "qgemm_avxvnni_gru_full_fused_fp16inout",
    "qgemm_avxvnni_int32",
    "qgemm_avxvnni_int32_k20",
];
/// Second runtime build: FastEnhancer-Small, symbol-prefixed `fe_s_*`.
fn build_fe_small(root: &Path, out: &Path) {
    let vendor = root.join("vendor/faster-enhancer");
    let cfg = root.join("cfg-s/fe_config_medium.h");
    println!("cargo:rerun-if-changed={}", cfg.display());

    let src = out.join("fe-src-s");
    copy_dir(&vendor, &src);

    // Injected CMakeLists: vendored one + target renamed + config override +
    // prefix definitions (public API + every colliding global).
    let mut cm = fs::read_to_string(src.join("CMakeLists.txt")).unwrap();
    cm = cm.replace("add_library(fe STATIC ${FE_SRC})", "add_library(fe_s STATIC ${FE_SRC})");
    cm = cm.replace("target_include_directories(fe\n", "target_include_directories(fe_s\n");
    cm = cm.replace("target_link_libraries(fe PUBLIC m)", "target_link_libraries(fe_s PUBLIC m)");
    cm = cm.replace("install(TARGETS fe\n", "install(TARGETS fe_s\n");
    let anchor = "target_compile_options(fe PRIVATE ${FE_FLAGS_COMMON})";
    assert!(cm.contains(anchor), "fe CMakeLists anchor not found");
    let prefix_defs = SYMS
        .iter()
        .map(|s| format!("{s}=fe_s_{s}"))
        .collect::<Vec<_>>()
        .join(" ");
    let inject = format!(
        "target_compile_options(fe_s PRIVATE ${{FE_FLAGS_COMMON}})\n\
         add_compile_definitions(fe_init=fe_s_init fe_run=fe_s_run fe_free=fe_s_free fe_reset=fe_s_reset)\n\
         add_compile_definitions({prefix_defs})\n\
         target_compile_options(fe_s PRIVATE -include \"{}\")\n", cfg.display()
    );
    cm = cm.replace(anchor, &inject);
    fs::write(src.join("CMakeLists.txt"), cm).unwrap();

    let build_dir = out.join("fe-build-s");
    cmake(&[
        "-S",
        src.to_str().unwrap(),
        "-B",
        build_dir.to_str().unwrap(),
        "-DCMAKE_BUILD_TYPE=Release",
        "-DFE_BUILD_TESTS=OFF",
        "-DFE_ENABLE_PROFILE=OFF",
    ])
    .unwrap_or_else(|e| panic!("fe_s cmake configure failed: {e}"));
    cmake(&["--build", build_dir.to_str().unwrap(), "--target", "fe_s", "--config", "Release"])
        .unwrap_or_else(|e| panic!("fe_s cmake build failed: {e}"));

    println!("cargo:rustc-link-search=native={}", build_dir.display());
    println!("cargo:rustc-link-lib=static=fe_s");
    println!("cargo:rustc-cfg=feature=\"fe_s_built\"");
}

/// MinGW cross-build for the Small runtime on Windows-MSVC: same symbol
/// prefixing as `build_fe_small` but with `-G Ninja -DCMAKE_C_COMPILER=gcc`.
/// Returns `Err` instead of panicking so the MSVC branch can degrade to
/// Medium-only or NS-only with a warning.
fn try_build_fe_small_mingw(root: &Path, out: &Path) -> Result<(), String> {
    let vendor = root.join("vendor/faster-enhancer");
    let cfg = root.join("cfg-s/fe_config_medium.h");
    println!("cargo:rerun-if-changed={}", cfg.display());

    let src = out.join("fe-src-s");
    copy_dir(&vendor, &src);

    let mut cm = fs::read_to_string(src.join("CMakeLists.txt")).map_err(|e| e.to_string())?;
    cm = cm.replace("add_library(fe STATIC ${FE_SRC})", "add_library(fe_s STATIC ${FE_SRC})");
    cm = cm.replace("target_include_directories(fe\n", "target_include_directories(fe_s\n");
    cm = cm.replace("target_link_libraries(fe PUBLIC m)", "target_link_libraries(fe_s PUBLIC m)");
    cm = cm.replace("install(TARGETS fe\n", "install(TARGETS fe_s\n");
    let anchor = "target_compile_options(fe PRIVATE ${FE_FLAGS_COMMON})";
    if !cm.contains(anchor) {
        return Err("fe CMakeLists anchor not found".into());
    }
    let prefix_defs = SYMS
        .iter()
        .map(|s| format!("{s}=fe_s_{s}"))
        .collect::<Vec<_>>()
        .join(" ");
    let inject = format!(
        "target_compile_options(fe_s PRIVATE ${{FE_FLAGS_COMMON}})\n\
         add_compile_definitions(fe_init=fe_s_init fe_run=fe_s_run fe_free=fe_s_free fe_reset=fe_s_reset)\n\
         add_compile_definitions({prefix_defs})\n\
         target_compile_options(fe_s PRIVATE -include \"{}\")\n",
        cfg.display()
    );
    cm = cm.replace(anchor, &inject);
    fs::write(src.join("CMakeLists.txt"), cm).map_err(|e| e.to_string())?;

    let build_dir = out.join("fe-build-s");
    cmake(&[
        "-S",
        src.to_str().unwrap(),
        "-B",
        build_dir.to_str().unwrap(),
        "-G",
        "Ninja",
        "-DCMAKE_C_COMPILER=gcc",
        "-DCMAKE_BUILD_TYPE=Release",
        "-DFE_BUILD_TESTS=OFF",
        "-DFE_ENABLE_PROFILE=OFF",
    ])?;
    cmake(&["--build", build_dir.to_str().unwrap(), "--target", "fe_s"])?;

    println!("cargo:rustc-link-search=native={}", build_dir.display());
    println!("cargo:rustc-link-lib=static=fe_s");
    Ok(())
}

fn main() {
    println!("cargo:rustc-check-cfg=cfg(feature, values(\"fe_built\", \"fe_s_built\"))");
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("vendor/faster-enhancer");
    println!("cargo:rerun-if-changed={}", root.display());

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();

    // Windows-MSVC: the app links with MSVC (webrtc-audio-processing requires
    // it), so fe is cross-compiled with MinGW gcc + Ninja here. fe produces a
    // GNU-format .a, which is consumed via lld-link (configured in
    // .cargo/config.toml as `linker=lld-link` — no env var needed). lld-link
    // reads both the MSVC .lib files (webrtc) and the GNU .a (fe). If gcc/ninja
    // are unavailable the build degrades to the WebRTC NS-only tier with a
    // warning — never a hard failure. No RUSTFLAGS gate: .cargo/config.toml
    // already ensures lld-link.
    if target_env.contains("msvc") {
        let build_dir = out.join("fe-build-mingw");
        let st = cmake(&[
            "-S",
            root.to_str().unwrap(),
            "-B",
            build_dir.to_str().unwrap(),
            "-G",
            "Ninja",
            "-DCMAKE_C_COMPILER=gcc",
            "-DCMAKE_BUILD_TYPE=Release",
            "-DFE_BUILD_TESTS=OFF",
            "-DFE_ENABLE_PROFILE=OFF",
        ])
        .and_then(|_| cmake(&["--build", build_dir.to_str().unwrap(), "--target", "fe"]));
        match st {
            Ok(()) => {
                println!("cargo:rustc-link-search=native={}", build_dir.display());
                println!("cargo:rustc-link-lib=static=fe");
                println!("cargo:rustc-cfg=feature=\"fe_built\"");
                // Second runtime: FastEnhancer-Small (fe_s_*) also via MinGW.
                let manifest_root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
                match try_build_fe_small_mingw(&manifest_root, &out) {
                    Ok(()) => {
                        println!("cargo:rustc-cfg=feature=\"fe_s_built\"");
                    }
                    Err(e) => println!(
                        "cargo:warning=lumen-voice: faster-enhancer Small runtime not built on MSVC ({e}); Medium tier only."
                    ),
                }
            }
            Err(e) => println!(
                "cargo:warning=lumen-voice: faster-enhancer C runtime not built on \
                 MSVC ({e}); WebRTC NS-only tier in effect. On CI ensure MinGW gcc \
                 + ninja are on PATH and linker is lld-link (via .cargo/config.toml)."
            ),
        }
        return;
    }

    let build_dir = out.join("fe-build");
    cmake(&[
        "-S",
        root.to_str().unwrap(),
        "-B",
        build_dir.to_str().unwrap(),
        "-DCMAKE_BUILD_TYPE=Release",
        "-DFE_BUILD_TESTS=OFF",
        "-DFE_ENABLE_PROFILE=OFF",
    ])
    .unwrap_or_else(|e| panic!("{e}"));
    cmake(&["--build", build_dir.to_str().unwrap(), "--target", "fe", "--config", "Release"])
        .unwrap_or_else(|e| panic!("{e}"));

    println!("cargo:rustc-link-search=native={}", build_dir.display());
    println!("cargo:rustc-link-lib=static=fe");
    println!("cargo:rustc-cfg=feature=\"fe_built\"");

    // Second runtime: FastEnhancer-Small (fe_s_*), the "Ligera" tier.
    build_fe_small(
        &PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()),
        &out,
    );

    if !env::var("CARGO_CFG_TARGET_OS").map(|o| o == "windows").unwrap_or(false) {
        println!("cargo:rustc-link-lib=m");
    }
}
