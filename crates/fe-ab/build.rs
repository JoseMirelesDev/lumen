//! fe-ab build: compiles the vendored faster-enhancer.c runtime TWICE with
//! the Small and Base 48 kHz configs (hop 512, dims from the released
//! onnx-48khz-v1 models), symbol-renamed (fe_s_* / fe_b_*) so both link
//! into one binary, plus a scalar tail layer that lets Base (C2=F2=36,
//! not 8-aligned) run — the production runtime's kernels have no
//! M%8/N%8 remainder path.
//!
//! The vendored source tree is copied into OUT_DIR (the copy gets the
//! fe_gru.c guard patch + the tail file + a generated CMakeLists); the
//! original vendor/ tree in lumen-voice is never touched.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const VENDOR: &str = "../lumen-voice/vendor/faster-enhancer";

/// Scalar tail + AVX2 dispatch wrappers. Compiled into every fe-ab build
/// (S and B); for Small it only ever forwards to the SIMD kernels (all
/// dims are 8-aligned), for Base it handles the 36-wide GEMMs/GRU.
const TAIL_C: &str = r#"
#include "qgemm_dispatch.h"
#include "fe_qgemm.h"
#include "qgemm/qgemm_arch.h"
#include <math.h>
#include <stdint.h>
#include <stdlib.h>

/* The renamed SIMD kernels (build.rs adds -Dqgemm_avx2_*=<name>_simd when
 * compiling qgemm_avx2.c; every other TU references the plain names and
 * resolves to these wrappers). */
void qgemm_avx2_int32_simd(int M, int N, int K, const int8_t *A_q, const int8_t *Bp, int32_t *C32, int ldc32);
void qgemm_avx2_fp32_fused_simd(int M, int N, int K, const int8_t *A_q, const int8_t *Bp, const float *combined_scale, const float *bias, float *C, int ldc, int act_silu, int32_t *c32_tail);
void qgemm_avx2_fp32_fused_acc_simd(int M, int N, int K, const int8_t *A_q, const int8_t *Bp, const float *combined_scale, const float *bias, float *C, int ldc, int32_t *c32_tail);
void qgemm_avx2_fp32_fused_track_maxabs_simd(int M, int N, int K, const int8_t *A_q, const int8_t *Bp, const float *combined_scale, const float *bias, float *C, int ldc, int32_t *c32_tail, float *max_abs_out);
void qgemm_avx2_gru_full_fused_fp16inout_simd(int M, int D, const int8_t *Xq, int ld_x, const int8_t *Hq, int ld_h, const int8_t *Wq_ih, const int8_t *Wq_hh, const float *combined_ih, const float *bias_eff_ih, const float *combined_hh, const float *bias_eff_hh, const float *br_sum, const float *bz_sum, const float *bn_i, const float *bn_h, uint16_t *h_inout_fp16, int ld_h_inout, float *h_out_scratch, int ld_h_out);

/* DOTPROD pack lookup: W[n][k] = Bp[(n/8)*k4g*32 + (k/4)*32 + (n%8)*4 + (k%4)] */
static inline int fe_pack_w(const int8_t *Bp, int k4_groups, int n, int k) {
    const int bn = n >> 3, lane = n & 7, g = k >> 2, kl = k & 3;
    return (int)Bp[((size_t)bn * k4_groups + g) * 32 + lane * 4 + kl];
}

static void fe_tail_int32(int M, int N, int K, const int8_t *A_q, const int8_t *Bp, int32_t *C32, int ldc32) {
    const int k4 = (K + 3) / 4;
    for (int m = 0; m < M; ++m) {
        const int8_t *a = A_q + (size_t)m * K;
        int32_t *c = C32 + (size_t)m * ldc32;
        for (int n = 0; n < N; ++n) {
            int32_t acc = 0;
            for (int k = 0; k < K; ++k) acc += (int32_t)a[k] * fe_pack_w(Bp, k4, n, k);
            c[n] = acc;
        }
    }
}

static inline float fe_tail_silu(float x) { return x / (1.0f + expf(-x)); }

static void fe_tail_fused_impl(int M, int N, int K, const int8_t *A_q, const int8_t *Bp,
                               const float *combined_scale, const float *bias,
                               float *C, int ldc, int act_silu, int accumulate,
                               float *max_out) {
    const int k4 = (K + 3) / 4;
    float mx = 0.0f;
    for (int m = 0; m < M; ++m) {
        const int8_t *a = A_q + (size_t)m * K;
        float *c = C + (size_t)m * ldc;
        for (int n = 0; n < N; ++n) {
            int32_t acc = 0;
            for (int k = 0; k < K; ++k) acc += (int32_t)a[k] * fe_pack_w(Bp, k4, n, k);
            float y = combined_scale[n] * (float)acc + (bias ? bias[n] : 0.0f);
            if (act_silu) y = fe_tail_silu(y);
            if (accumulate) c[n] += y; else c[n] = y;
            if (max_out) { const float ab = fabsf(y); if (ab > mx) mx = ab; }
        }
    }
    if (max_out) *max_out = mx;
}

/* fp16 round-trip for the GRU state (RN-even; double-based, correct and
 * simple — only used by the Base tail path). */
static inline uint16_t fe_tail_f32_to_f16(float x) {
    double d = (double)x;
    if (d != d) return 0x7e00u;                    /* NaN */
    if (d >= 65520.0) return 0x7c00u;              /* +inf */
    if (d <= -65520.0) return 0xfc00u;             /* -inf */
    long r = lrint(d * 16777216.0);                /* * 2^24, half-even */
    if (r == 0) return (uint16_t)(d < 0.0 ? 0x8000u : 0x0000u);
    if (r >= (1L << 25) || r <= -(1L << 25))       /* overflow -> inf */
        return (uint16_t)(d < 0.0 ? 0xfc00u : 0x7c00u);
    uint32_t rr = (uint32_t)(r < 0 ? -r : r);
    if (rr < (1u << 10))                           /* fp16 subnormal: bits direct */
        return (uint16_t)((d < 0.0 ? 0x8000u : 0u) | rr);
    uint32_t e = (uint32_t)(31 - __builtin_clz(rr)) - 9;
    return (uint16_t)((d < 0.0 ? 0x8000u : 0u) | (e << 10) | (rr & 0x3ffu));
}

static inline float fe_tail_f16_to_f32(uint16_t h) {
    const uint32_t sign = ((uint32_t)h & 0x8000u) << 16;
    const uint32_t e = (h >> 10) & 0x1fu;
    const uint32_t m = h & 0x3ffu;
    uint32_t u;
    if (e == 0) {
        if (m == 0) { u = sign; }
        else {
            int ee = -14;
            uint32_t mm = m;
            while (!(mm & 0x400u)) { mm <<= 1; ee -= 1; }
            u = sign | (uint32_t)(ee + 127) << 23 | ((mm & 0x3ffu) << 13);
        }
    } else if (e == 0x1f) {
        u = sign | 0x7f800000u | (m << 13);
    } else {
        u = sign | ((e + 127 - 15) << 23) | (m << 13);
    }
    union { uint32_t u; float f; } out = { u };
    return out.f;
}

static void fe_tail_gru(int M, int D,
                        const int8_t *Xq, int ld_x, const int8_t *Hq, int ld_h,
                        const int8_t *Wq_ih, const int8_t *Wq_hh,
                        const float *combined_ih, const float *bias_eff_ih,
                        const float *combined_hh, const float *bias_eff_hh,
                        const float *br_sum, const float *bz_sum,
                        const float *bn_i, const float *bn_h,
                        uint16_t *h_inout_fp16, int ld_h_inout,
                        float *h_out_scratch, int ld_h_out) {
    const int k4 = (D + 3) / 4;
    float ih[3 * 128];   /* D <= FE_QGEMM_MAX_GRU_D (128) */
    for (int m = 0; m < M; ++m) {
        const int8_t *xq = Xq + (size_t)m * ld_x;
        const int8_t *hq = Hq + (size_t)m * ld_h;
        uint16_t *h16 = h_inout_fp16 + (size_t)m * ld_h_inout;
        float *hout = h_out_scratch + (size_t)m * ld_h_out;
        for (int g = 0; g < 3; ++g) {
            const int go = g * D;
            for (int n = 0; n < D; ++n) {
                int32_t acc = 0;
                for (int k = 0; k < D; ++k)
                    acc += (int32_t)xq[k] * fe_pack_w(Wq_ih, k4, go + n, k);
                ih[go + n] = combined_ih[go + n] * (float)acc + bias_eff_ih[go + n];
            }
        }
        for (int n = 0; n < D; ++n) {
            int32_t ar = 0, az = 0, an = 0;
            for (int k = 0; k < D; ++k) {
                const int32_t h = hq[k];
                ar += h * fe_pack_w(Wq_hh, k4, n, k);
                az += h * fe_pack_w(Wq_hh, k4, D + n, k);
                an += h * fe_pack_w(Wq_hh, k4, 2 * D + n, k);
            }
            const float hr = combined_hh[n] * (float)ar + bias_eff_hh[n];
            const float hz = combined_hh[D + n] * (float)az + bias_eff_hh[D + n];
            const float hn = combined_hh[2 * D + n] * (float)an + bias_eff_hh[2 * D + n];
            const float r = fe_tail_silu(ih[n] + hr + br_sum[n]);
            const float z = fe_tail_silu(ih[D + n] + hz + bz_sum[n]);
            const float ng = tanhf(ih[2 * D + n] + bn_i[n] + r * (hn + bn_h[n]));
            const float ho = fe_tail_f16_to_f32(h16[n]);
            const float hnew = z * (ho - ng) + ng;   /* == z*ho + (1-z)*ng */
            hout[n] = hnew;
            h16[n] = fe_tail_f32_to_f16(hnew);
        }
    }
}

/* ---- dispatch wrappers: SIMD when tile-aligned, scalar tail otherwise ---- */
void qgemm_avx2_int32(int M, int N, int K, const int8_t *A_q, const int8_t *Bp, int32_t *C32, int ldc32) {
    if ((M & 7) == 0 && (N & 7) == 0) { qgemm_avx2_int32_simd(M, N, K, A_q, Bp, C32, ldc32); return; }
    fe_tail_int32(M, N, K, A_q, Bp, C32, ldc32);
}

void qgemm_avx2_fp32_fused(int M, int N, int K, const int8_t *A_q, const int8_t *Bp,
                           const float *combined_scale, const float *bias,
                           float *C, int ldc, int act_silu, int32_t *c32_tail) {
    if ((M & 7) == 0 && (N & 7) == 0) {
        qgemm_avx2_fp32_fused_simd(M, N, K, A_q, Bp, combined_scale, bias, C, ldc, act_silu, c32_tail);
        return;
    }
    (void)c32_tail;
    fe_tail_fused_impl(M, N, K, A_q, Bp, combined_scale, bias, C, ldc, act_silu, 0, NULL);
}

void qgemm_avx2_fp32_fused_acc(int M, int N, int K, const int8_t *A_q, const int8_t *Bp,
                               const float *combined_scale, const float *bias,
                               float *C, int ldc, int32_t *c32_tail) {
    if ((M & 7) == 0 && (N & 7) == 0) {
        qgemm_avx2_fp32_fused_acc_simd(M, N, K, A_q, Bp, combined_scale, bias, C, ldc, c32_tail);
        return;
    }
    (void)c32_tail;
    fe_tail_fused_impl(M, N, K, A_q, Bp, combined_scale, bias, C, ldc, 0, 1, NULL);
}

void qgemm_avx2_fp32_fused_track_maxabs(int M, int N, int K, const int8_t *A_q, const int8_t *Bp,
                                        const float *combined_scale, const float *bias,
                                        float *C, int ldc, int32_t *c32_tail,
                                        float *max_abs_out) {
    if ((M & 7) == 0 && (N & 7) == 0) {
        qgemm_avx2_fp32_fused_track_maxabs_simd(M, N, K, A_q, Bp, combined_scale, bias, C, ldc, c32_tail, max_abs_out);
        return;
    }
    (void)c32_tail;
    fe_tail_fused_impl(M, N, K, A_q, Bp, combined_scale, bias, C, ldc, 0, 0, max_abs_out);
}

void qgemm_avx2_gru_full_fused_fp16inout(int M, int D,
                                         const int8_t *Xq, int ld_x,
                                         const int8_t *Hq, int ld_h,
                                         const int8_t *Wq_ih, const int8_t *Wq_hh,
                                         const float *combined_ih, const float *bias_eff_ih,
                                         const float *combined_hh, const float *bias_eff_hh,
                                         const float *br_sum, const float *bz_sum,
                                         const float *bn_i, const float *bn_h,
                                         uint16_t *h_inout_fp16, int ld_h_inout,
                                         float *h_out_scratch, int ld_h_out) {
    if ((M & 7) == 0 && (D & 7) == 0) {
        qgemm_avx2_gru_full_fused_fp16inout_simd(M, D, Xq, ld_x, Hq, ld_h, Wq_ih, Wq_hh,
                                                 combined_ih, bias_eff_ih, combined_hh, bias_eff_hh,
                                                 br_sum, bz_sum, bn_i, bn_h,
                                                 h_inout_fp16, ld_h_inout, h_out_scratch, ld_h_out);
        return;
    }
    fe_tail_gru(M, D, Xq, ld_x, Hq, ld_h, Wq_ih, Wq_hh,
                combined_ih, bias_eff_ih, combined_hh, bias_eff_hh,
                br_sum, bz_sum, bn_i, bn_h,
                h_inout_fp16, ld_h_inout, h_out_scratch, ld_h_out);
}
"#;

/// Global symbols defined by the runtime (from `nm --defined-only`), minus
/// the already-prefixed API (fe_s_*). Both libs link into one binary, so
/// every colliding global gets a per-variant `-D` rename; references inside
/// each lib rename consistently and libc calls are untouched.
const SYMS: &str = r#"
fe_conv1d_k1_FCin
fe_conv1d_k1_silu_concat2_fp16b
fe_conv1d_k3_buf_silu
fe_conv1d_k3_buf_silu_skip_fp16
fe_conv1d_k3_winograd_silu
fe_conv1d_k3_winograd_silu_skip_fp16
fe_conv_transpose1d
fe_cpu_brand
fe_cpu_x86_caps
fe_fft_forward_avx2
fe_fft_forward_avx512
fe_fft_init
fe_fft_inverse_avx2
fe_fft_inverse_avx512
fe_fft_plan_init
fe_free_weights
fe_gru_r_band
fe_gru_step_fp16h
fe_gru_z_band
fe_irfft
fe_istft
fe_load_weights
fe_mhsa
fe_now_us
fe_pack_W
fe_process_frame
fe_profile_dump
fe_profile_record
fe_profile_reset
fe_qgemm_build_bias_eff_nobias
fe_qgemm_compute_row_sums
fe_qgemm_force_tier
fe_qgemm_i8mm_repack_block
fe_qgemm_init
fe_qgemm_ops
fe_qgemm_packed_calib_acc
fe_qgemm_packed_calib_to_int8out
fe_qgemm_packed_calib_transposed_in
fe_qgemm_packed_calib_transposed_in_fp16
fe_qgemm_packed_silu_calib_concat2_fp16b
fe_qgemm_pack_W
fe_qgemm_prequant
fe_qgemm_repack_i8mm
fe_qgemm_tail_unsupported
fe_qg_x86_avx512
fe_quantize_activation
fe_quantize_activation_fp16
fe_quantize_activation_transposed
fe_quantize_activation_transposed_fp16
fe_quantize_activation_with_scale
fe_rfft
fe_sgemm_packed
fe_sgemm_packed_bias
fe_silu_skip_fp16
fe_softmax_rows_quant_from_int32
fe_state_create
fe_state_destroy
fe_stft
fe_stft_init
fe_strided_conv1d
fe_vec_add
fe_weights_finalize_for_tier
fe_winograd_f23_derive_weights
fe_winograd_set_scratch
fft_avx2_radix4_stage_wide
fft_x86_radix4_stage
g_fft_plan
qgemm_avx2_fp32_fused
qgemm_avx2_fp32_fused_acc
qgemm_avx2_fp32_fused_acc_simd
qgemm_avx2_fp32_fused_simd
qgemm_avx2_fp32_fused_track_maxabs
qgemm_avx2_fp32_fused_track_maxabs_simd
qgemm_avx2_gru_full_fused_fp16inout
qgemm_avx2_gru_full_fused_fp16inout_simd
qgemm_avx2_int32
qgemm_avx2_int32_k20
qgemm_avx2_int32_simd
qgemm_avx2_prefault_buffers
qgemm_avx512vnni_fp32_fused
qgemm_avx512vnni_fp32_fused_acc
qgemm_avx512vnni_fp32_fused_track_maxabs
qgemm_avx512vnni_gru_full_fused_fp16inout
qgemm_avx512vnni_int32
qgemm_avx512vnni_int32_k20
qgemm_avxvnni_fp32_fused
qgemm_avxvnni_fp32_fused_acc
qgemm_avxvnni_fp32_fused_track_maxabs
qgemm_avxvnni_gru_full_fused_fp16inout
qgemm_avxvnni_int32
qgemm_avxvnni_int32_k20
"#;

fn prefix_flags(sym: &str) -> String {
    SYMS
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|n| format!("-D{n}={sym}_{n}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn cmake_template(variant: &str, sym: &str, cfg_abs: &str) -> String {
    let prefix = prefix_flags(sym);
    format!(
        r#"cmake_minimum_required(VERSION 3.20)
project(fe_ab_{variant} LANGUAGES C)
set(CMAKE_C_STANDARD 11)
set(CMAKE_C_STANDARD_REQUIRED ON)
set(CMAKE_BUILD_TYPE Release CACHE STRING "Build type" FORCE)

set(FE_CFG_INCLUDE "{cfg_abs}")
set(FE_SRC
    src/fe_engine.c src/fe_fft.c src/fe_pipeline.c src/fe_profile.c
    src/fe_stft.c src/fe_weights.c
    src/nn/fe_activations.c src/nn/fe_attention.c src/nn/fe_conv.c
    src/nn/fe_gru.c src/nn/fe_qgemm.c src/nn/fe_sgemm.c src/nn/fe_vec.c
    src/qgemm/qgemm_dispatch.c src/qgemm/qgemm_pack.c src/qgemm/qgemm_quant.c
    src/qgemm/x86/qgemm_avx2.c src/qgemm/x86/qgemm_avxvnni.c
    src/qgemm/x86/qgemm_avx512vnni.c src/qgemm/x86/cpu_x86.c
    src/winograd/fe_winograd.c
    src/fft/fft_avx2.c src/fft/fft_avx512.c
    src/qgemm/qgemm_scalar_tail.c)

add_library({sym} STATIC ${{FE_SRC}})
target_include_directories({sym} PRIVATE include src src/internal)
target_compile_options({sym} PRIVATE
    -O3 -g -Wall -Wextra -Wno-unused-parameter -fno-math-errno -fno-trapping-math
    -ffinite-math-only -fmerge-all-constants -fno-exceptions
    -fno-stack-protector -fvisibility=hidden -DNDEBUG -DFE_ENABLE_PROFILE
    -Dfe_init={sym}_init -Dfe_run={sym}_run
    -Dfe_free={sym}_free -Dfe_reset={sym}_reset
    -include ${{FE_CFG_INCLUDE}})
target_link_libraries({sym} PUBLIC m)

# Prefix every colliding global symbol per lib (see SYMS in build.rs).
set(FE_PREFIX_FLAGS {prefix})
target_compile_options({sym} PRIVATE ${{FE_PREFIX_FLAGS}})

# The AVX2 tier kernel TU gets its GEMM/GRU entry points renamed to
# {sym}_*_simd (the prefixed name the scalar-tail wrappers call). These
# per-file -D come after the global FE_PREFIX_FLAGS on the command line,
# so they win for this TU.
set_source_files_properties(src/qgemm/x86/qgemm_avx2.c PROPERTIES COMPILE_FLAGS
    "-mavx2 -mfma -Dqgemm_avx2_int32={sym}_qgemm_avx2_int32_simd -Dqgemm_avx2_fp32_fused={sym}_qgemm_avx2_fp32_fused_simd -Dqgemm_avx2_fp32_fused_acc={sym}_qgemm_avx2_fp32_fused_acc_simd -Dqgemm_avx2_fp32_fused_track_maxabs={sym}_qgemm_avx2_fp32_fused_track_maxabs_simd -Dqgemm_avx2_gru_full_fused_fp16inout={sym}_qgemm_avx2_gru_full_fused_fp16inout_simd")
set_source_files_properties(src/fft/fft_avx2.c PROPERTIES COMPILE_FLAGS "-mavx2 -mfma")
set_source_files_properties(src/qgemm/x86/qgemm_avxvnni.c PROPERTIES COMPILE_FLAGS "-mavx2 -mfma -mavxvnni")
set_source_files_properties(src/qgemm/x86/qgemm_avx512vnni.c PROPERTIES COMPILE_FLAGS "-mavx2 -mfma -mavxvnni -mavx512f -mavx512bw -mavx512vl -mavx512vnni")
set_source_files_properties(src/fft/fft_avx512.c PROPERTIES COMPILE_FLAGS "-mavx2 -mfma -mavxvnni -mavx512f -mavx512bw -mavx512vl -mavx512vnni")
set_source_files_properties(
    src/winograd/fe_winograd.c src/qgemm/qgemm_quant.c src/nn/fe_activations.c
    src/nn/fe_attention.c src/fe_engine.c src/nn/fe_gru.c src/nn/fe_qgemm.c
    src/nn/fe_sgemm.c src/fe_stft.c src/nn/fe_vec.c
    PROPERTIES COMPILE_FLAGS "-mavx2 -mfma -mf16c")
"#,
        variant = variant,
        sym = sym,
        cfg_abs = cfg_abs,
    )
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for e in fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let t = e.path();
        let o = dst.join(e.file_name());
        if t.is_dir() {
            copy_dir(&t, &o);
        } else {
            fs::copy(&t, &o).unwrap();
        }
    }
}

fn cmake(dir: &Path, args: &[&str]) {
    let st = Command::new("cmake").current_dir(dir).args(args).status().unwrap();
    assert!(st.success(), "cmake failed in {}", dir.display());
}

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let vendor = root.join(VENDOR);
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    println!("cargo:rerun-if-changed={}", vendor.display());
    println!("cargo:rerun-if-changed={}", root.join("cfg-s/fe_config_medium.h").display());
    println!("cargo:rerun-if-changed={}", root.join("cfg-b/fe_config_medium.h").display());

    for (variant, sym, cfg) in [
        ("s", "fe_s", "cfg-s"),
        ("b", "fe_b", "cfg-b"),
        ("m", "fe_m", "cfg-m"),
    ] {
        let src = out.join(format!("fe-src-{variant}"));
        copy_dir(&vendor, &src);

        // Patch fe_gru.c: drop the (D&7)==0 && (freq&7)==0 guard on the x86
        // branch so Base (D=36) reaches the (now dim-dispatching) kernel.
        let gru = src.join("src/nn/fe_gru.c");
        let gru_src = fs::read_to_string(&gru).unwrap();
        let anchor = "    if ((fe_qgemm_ops.tier == FE_QGEMM_TIER_X86_AVX512_VNNI\n         || fe_qgemm_ops.tier == FE_QGEMM_TIER_X86_AVX_VNNI\n         || fe_qgemm_ops.tier == FE_QGEMM_TIER_X86_AVX2)\n        && (D & 7) == 0 && (freq & 7) == 0) {";
        let fixed = "    if (fe_qgemm_ops.tier == FE_QGEMM_TIER_X86_AVX512_VNNI\n         || fe_qgemm_ops.tier == FE_QGEMM_TIER_X86_AVX_VNNI\n         || fe_qgemm_ops.tier == FE_QGEMM_TIER_X86_AVX2) {";
        assert!(gru_src.contains(anchor), "fe_gru.c patch anchor not found");
        fs::write(&gru, gru_src.replace(anchor, fixed)).unwrap();

        // Patch fe_qgemm.c: g_combined_scale / g_bias_eff_buf are sized
        // 3*FE_C2 (the GRU combined path needs 3*D floats), but the linear
        // transposed-in path writes N floats with N = FE_F1 (rf_post_lin:
        // M=C2, N=F1, K=F2). For Base (C2=36 -> 3*C2=108 < F1=128) both
        // buffers overrun by 20 floats; g_combined_scale lands on
        // fe_qgemm_ops in the linked binary and the next indirect call
        // SIGSEGVs (garbage function pointers). Small/Medium: 3*C2=144 >=
        // 128, so max() leaves them unchanged.
        let qgemm = src.join("src/nn/fe_qgemm.c");
        let qgemm_src = fs::read_to_string(&qgemm).unwrap();
        let qanchor = "#define FE_QGEMM_MAX_N (3 * FE_C2)";
        let qfixed = "#define FE_QGEMM_MAX_N ((FE_F1 > 3 * FE_C2) ? FE_F1 : (3 * FE_C2))";
        assert!(qgemm_src.contains(qanchor), "fe_qgemm.c patch anchor not found");
        fs::write(&qgemm, qgemm_src.replace(qanchor, qfixed)).unwrap();

        // Patch fe_attention.c: with the padded config (C2/F2 -> 40) the
        // compiled FE_HEAD_DIM is 10 but the real model head dim is 9 --
        // the 1/sqrt(HD) score temperature must stay 1/sqrt(9) for the
        // padded attention to be mathematically identical. Small/Medium
        // define FE_HEAD_DIM_REAL == FE_HEAD_DIM, so this is a no-op.
        let attn = src.join("src/nn/fe_attention.c");
        let attn_src = fs::read_to_string(&attn).unwrap();
        let a1 = "    const float scale = 1.0f / sqrtf((float)HD);";
        let f1 = "    const float scale = 1.0f / sqrtf((float)FE_HEAD_DIM_REAL);";
        assert!(attn_src.contains(a1), "fe_attention.c scale anchor not found");
        fs::write(&attn, attn_src.replace(a1, f1)).unwrap();
        let attn_src = fs::read_to_string(&attn).unwrap();

        // Patch fe_attention.c: the padded key rows (freq 36 -> 40) have
        // exactly zero Q/K activations, so their logits are 0 and exp(0)
        // would add (cols - real) terms to every softmax row sum. Clamp
        // those logits to a large negative so exp() underflows to ~0.
        // No-op when FE_F2_REAL == freq (Small/Medium).
        let a2 = "        FE_TIME_END();\n        FE_TIME(\"14a_softmax_quant\",";
        let f2 = "        FE_TIME_END();\n        /* Padded freq rows (Base: 36 -> 40) have zero Q/K activations, so\n         * their logits are exactly 0; exp(0) would pollute the softmax\n         * row normalization. Clamp to a large negative so exp() ~ 0. */\n        for (int r = 0; r < freq; ++r) {\n            for (int c = FE_F2_REAL; c < freq; ++c)\n                c32[(size_t)r * freq + c] = -1073741824;\n        }\n        FE_TIME(\"14a_softmax_quant\",";
        assert!(attn_src.contains(a2), "fe_attention.c softmax anchor not found");
        fs::write(&attn, attn_src.replace(a2, f2)).unwrap();

        // Patch fe_engine.c: with the padded freq dim (Base: F2 36 -> 40)
        // the padded query rows (36-39) get non-zero attention/GRU output:
        // their zero Q row gives a UNIFORM softmax (exp(0) over all keys),
        // and the GRU gates fire from the real biases even with zero input.
        // Without correction those rows of rf_b become non-zero, then act
        // as non-zero attention KEYS for the next block and pollute the
        // real queries' softmax normalization. Zero them after each block.
        // No-op when FE_F2_REAL == FE_F2 (Small/Medium).
        let engine = src.join("src/fe_engine.c");
        let engine_src = fs::read_to_string(&engine).unwrap();
        let e1 = "                        s->attn_scoresq, FE_F2,\n                        s->qgemm_aq, s->qgemm_c32));\n    }";
        let f1 = "                        s->attn_scoresq, FE_F2,\n                        s->qgemm_aq, s->qgemm_c32));\n        /* Padded freq rows (Base: 36 -> 40): zero Q rows give a uniform\n         * softmax and the GRU gates fire from real biases, so both would\n         * write non-zero rf_b rows that become non-zero attention keys\n         * for the next block. Keep them exactly zero. */\n        for (int r = FE_F2_REAL; r < FE_F2; ++r)\n            memset(s->rf_b + (size_t)r * FE_C2, 0, (size_t)FE_C2 * sizeof(float));\n    }";
        assert!(engine_src.contains(e1), "fe_engine.c rf_b zeroing anchor not found");
        fs::write(&engine, engine_src.replace(e1, f1)).unwrap();

        fs::write(src.join("src/qgemm/qgemm_scalar_tail.c"), TAIL_C).unwrap();
        fs::write(src.join("CMakeLists.txt"), cmake_template(variant, sym, &root.join(cfg).join("fe_config_medium.h").display().to_string())).unwrap();

        let build = out.join(format!("fe-build-{variant}"));
        cmake(&src, &["-S", src.to_str().unwrap(), "-B", build.to_str().unwrap(), "-DCMAKE_BUILD_TYPE=Release"]);
        cmake(&src, &["--build", build.to_str().unwrap(), "--target", sym, "--config", "Release", "-j"]);

        println!("cargo:rustc-link-search=native={}", build.display());
        println!("cargo:rustc-link-lib=static={sym}");
    }
    println!("cargo:rustc-link-lib=m");
}
