/* fp16 (IEEE 754 binary16 / S1E5M10) helpers for runtime state storage.
 *
 * Hardware fp16 conversion is GUARANTEED on the fast tiers:
 *   x86 AVX2+FMA3+F16C — VCVTPS2PH / VCVTPH2PS.
 *   ARM: NEON baseline (aarch64) mandates FCVTN / FCVTL.
 *
 * SSE4.1 fallback (Pentium/Celeron, no F16C) uses software conversion
 * (scalar + 4-wide SSE helpers). Bit-id is not guaranteed vs hardware
 * tier but functional correctness is preserved; no SIGILL.
 *
 * Dimension contract: all engine call sites pass n that is a multiple of
 * the vector width (8 on AVX2 x86, 4 on SSE4.1 x86 and ARM).
 */
#ifndef FE_FP16_H
#define FE_FP16_H

#include <stdint.h>
#include <stdlib.h>   /* abort */
#include <string.h>   /* memcpy for soft fp16 */

#if defined(__x86_64__) || defined(_M_X64) || defined(__i386__) || defined(_M_IX86)
#  include <immintrin.h>
#elif defined(__aarch64__) || defined(__arm64__) || defined(_M_ARM64)
#  include <arm_neon.h>
#else
#  error "fe_fp16.h requires x86_64 (AVX2/FMA3/F16C or SSE4.1) or aarch64 (NEON FCVT)"
#endif

/* Single-line guard for callers: any leftover after the SIMD loop is a
 * dimension contract violation; abort loudly. */
static inline void fe_fp16_alignment_violation(void) { abort(); }

/* ---------------------------------------------------------------- *
 *  Vector SIMD conversion (hardware path)
 * ---------------------------------------------------------------- */

#if defined(__x86_64__) || defined(_M_X64) || defined(__i386__) || defined(_M_IX86)

/* Store 8 fp32 lanes as 8 fp16 (RNE). */
__attribute__((target("avx2,f16c,fma")))
static inline void fe_fp16_store8(uint16_t *dst, __m256 v) {
    _mm_storeu_si128((__m128i *)dst,
                     _mm256_cvtps_ph(v,
                         _MM_FROUND_TO_NEAREST_INT | _MM_FROUND_NO_EXC));
}

/* Load 8 fp16 lanes and expand to fp32. */
__attribute__((target("avx2,f16c,fma")))
static inline __m256 fe_fp16_load8(const uint16_t *src) {
    return _mm256_cvtph_ps(_mm_loadu_si128((const __m128i *)src));
}

#endif /* x86 hardware */

/* ---------------------------------------------------------------- *
 *  Software fp16 fallback for SSE4.1 (no F16C)
 * ---------------------------------------------------------------- */
#if defined(FE_QGEMM_HAVE_SSE41) && !defined(FE_QGEMM_HAVE_AVX2)
static inline uint16_t fe_fp16_from_f32_soft(float f) {
    uint32_t x;
    memcpy(&x, &f, 4);
    uint32_t sign = (x >> 16) & 0x8000;
    int32_t exp = ((x >> 23) & 0xFF) - 127 + 15;
    uint32_t frac = (x >> 13) & 0x3FF;
    if (exp <= 0) return (uint16_t)sign;  // underflow to zero
    if (exp >= 31) return (uint16_t)(sign | 0x7C00);  // overflow to inf
    return (uint16_t)(sign | (exp << 10) | frac);
}
static inline float fe_fp16_to_f32_soft(uint16_t h) {
    uint32_t sign = (h & 0x8000) << 16;
    uint32_t exp = (h >> 10) & 0x1F;
    uint32_t frac = h & 0x3FF;
    if (exp == 0) { float f = 0.0f; uint32_t r = sign; memcpy(&f, &r, 4); return f; }
    if (exp == 31) { uint32_t r = sign | 0x7F800000 | (frac << 13); float f; memcpy(&f, &r, 4); return f; }
    uint32_t r = sign | ((exp - 15 + 127) << 23) | (frac << 13);
    float f; memcpy(&f, &r, 4); return f;
}
// SSE4.1 4-wide store: convert 4 fp32 to 4 fp16
static inline void fe_fp16_store4_soft(uint16_t *dst, __m128 v) {
    float tmp[4];
    _mm_storeu_ps(tmp, v);
    for (int i = 0; i < 4; i++) dst[i] = fe_fp16_from_f32_soft(tmp[i]);
}
// SSE4.1 4-wide load: convert 4 fp16 to 4 fp32
static inline __m128 fe_fp16_load4_soft(const uint16_t *src) {
    float tmp[4];
    for (int i = 0; i < 4; i++) tmp[i] = fe_fp16_to_f32_soft(src[i]);
    return _mm_loadu_ps(tmp);
}
// 8-wide split helpers for SSE4.1 (two 4-wide ops)
static inline void fe_fp16_store8_sse41(uint16_t *dst, __m128 v_lo, __m128 v_hi) {
    fe_fp16_store4_soft(dst, v_lo);
    fe_fp16_store4_soft(dst + 4, v_hi);
}
static inline void fe_fp16_load8_split(const uint16_t *src, __m128 *lo, __m128 *hi) {
    *lo = fe_fp16_load4_soft(src);
    *hi = fe_fp16_load4_soft(src + 4);
}
#endif /* SSE4.1 soft */

#if defined(__aarch64__) || defined(__arm64__) || defined(_M_ARM64)

/* Store 4 fp32 lanes as 4 fp16. */
static inline void fe_fp16_store4(uint16_t *dst, float32x4_t v) {
    float16x4_t h = vcvt_f16_f32(v);
    vst1_u16(dst, vreinterpret_u16_f16(h));
}

/* Load 4 fp16 lanes and expand to fp32. */
static inline float32x4_t fe_fp16_load4(const uint16_t *src) {
    float16x4_t h = vreinterpret_f16_u16(vld1_u16(src));
    return vcvt_f32_f16(h);
}

#endif /* aarch64 */

/* ---------------------------------------------------------------- *
 *  Buffer-level conversions
 *  Caller contract: n must be a multiple of the SIMD width
 *  (8 on AVX2 x86, 4 on SSE41 x86 / ARM). Violation → abort().
 * ---------------------------------------------------------------- */

/* fp32 [n] -> fp16 [n]. */
#if defined(__x86_64__) || defined(_M_X64) || defined(__i386__) || defined(_M_IX86)
__attribute__((target("avx2,f16c,fma")))
#endif
static inline void fe_fp16_pack_buf(uint16_t *dst, const float *src, int n) {
    int i = 0;
#if defined(__x86_64__) || defined(_M_X64) || defined(__i386__) || defined(_M_IX86)
#if defined(FE_QGEMM_HAVE_SSE41) && !defined(FE_QGEMM_HAVE_AVX2)
    for (; i + 3 < n; i += 4) {
        __m128 v = _mm_loadu_ps(src + i);
        fe_fp16_store4_soft(dst + i, v);
    }
#else
    for (; i + 7 < n; i += 8) {
        __m256 v = _mm256_loadu_ps(src + i);
        fe_fp16_store8(dst + i, v);
    }
#endif
#elif defined(__aarch64__) || defined(__arm64__) || defined(_M_ARM64)
    for (; i + 3 < n; i += 4) {
        float32x4_t v = vld1q_f32(src + i);
        fe_fp16_store4(dst + i, v);
    }
#endif
    if (i != n) fe_fp16_alignment_violation();
}

/* fp16-input min/max scan. Reads fp16 once with on-the-fly cvt; no
 * fp32 materialization. Output matches fe_qg_min_max signature. */
#if defined(__x86_64__) || defined(_M_X64) || defined(__i386__) || defined(_M_IX86)
__attribute__((target("avx2,f16c,fma")))
#endif
static inline void fe_fp16_min_max(const uint16_t *p, int n,
                                    float *min_out, float *max_out) {
    if (n <= 0) { *min_out = 0.0f; *max_out = 0.0f; return; }
    int i = 0;
    float vmin = 0.0f, vmax = 0.0f;
#if defined(__x86_64__) || defined(_M_X64) || defined(__i386__) || defined(_M_IX86)
#if defined(FE_QGEMM_HAVE_SSE41) && !defined(FE_QGEMM_HAVE_AVX2)
    {
        __m128 v0 = fe_fp16_load4_soft(p);
        __m128 vmn = v0, vmx = v0;
        i = 4;
        for (; i + 3 < n; i += 4) {
            __m128 v = fe_fp16_load4_soft(p + i);
            vmn = _mm_min_ps(vmn, v);
            vmx = _mm_max_ps(vmx, v);
        }
        __m128 mn = _mm_min_ps(vmn, _mm_movehl_ps(vmn, vmn));
        mn = _mm_min_ps(mn, _mm_shuffle_ps(mn, mn, 1));
        // Actually need scalar reduction: _mm_min_ss
        mn = _mm_min_ss(mn, _mm_shuffle_ps(mn, mn, _MM_SHUFFLE(0,0,0,1)));
        vmin = _mm_cvtss_f32(mn);
        __m128 mx = _mm_max_ps(vmx, _mm_movehl_ps(vmx, vmx));
        mx = _mm_max_ps(mx, _mm_shuffle_ps(mx, mx, 1));
        mx = _mm_max_ss(mx, _mm_shuffle_ps(mx, mx, _MM_SHUFFLE(0,0,0,1)));
        vmax = _mm_cvtss_f32(mx);
    }
#else
    {
        __m256 v0 = fe_fp16_load8(p);
        __m256 vmn = v0, vmx = v0;
        i = 8;
        for (; i + 7 < n; i += 8) {
            __m256 v = fe_fp16_load8(p + i);
            vmn = _mm256_min_ps(vmn, v);
            vmx = _mm256_max_ps(vmx, v);
        }
        __m128 lo = _mm256_castps256_ps128(vmn);
        __m128 hi = _mm256_extractf128_ps(vmn, 1);
        __m128 mn = _mm_min_ps(lo, hi);
        mn = _mm_min_ps(mn, _mm_movehl_ps(mn, mn));
        mn = _mm_min_ss(mn, _mm_shuffle_ps(mn, mn, 1));
        vmin = _mm_cvtss_f32(mn);
        lo = _mm256_castps256_ps128(vmx);
        hi = _mm256_extractf128_ps(vmx, 1);
        __m128 mx = _mm_max_ps(lo, hi);
        mx = _mm_max_ps(mx, _mm_movehl_ps(mx, mx));
        mx = _mm_max_ss(mx, _mm_shuffle_ps(mx, mx, 1));
        vmax = _mm_cvtss_f32(mx);
    }
#endif
#elif defined(__aarch64__) || defined(__arm64__) || defined(_M_ARM64)
    {
        float32x4_t v0 = fe_fp16_load4(p);
        float32x4_t vmn = v0, vmx = v0;
        i = 4;
        for (; i + 3 < n; i += 4) {
            float32x4_t v = fe_fp16_load4(p + i);
            vmn = vminq_f32(vmn, v);
            vmx = vmaxq_f32(vmx, v);
        }
        vmin = vminvq_f32(vmn);
        vmax = vmaxvq_f32(vmx);
    }
#endif
    if (i != n) fe_fp16_alignment_violation();
    *min_out = vmin;
    *max_out = vmax;
}

/* fp16-input asymmetric quantize. q = clamp(round(x*inv) + zp_off). */
#if defined(__x86_64__) || defined(_M_X64) || defined(__i386__) || defined(_M_IX86)
__attribute__((target("avx2,f16c,fma")))
#endif
static inline void fe_fp16_quantize_asym(const uint16_t *p, int n,
                                          int8_t *out, float inv,
                                          int32_t zp_off) {
    int i = 0;
    const float zof = (float)zp_off;
#if defined(__x86_64__) || defined(_M_X64) || defined(__i386__) || defined(_M_IX86)
#if defined(FE_QGEMM_HAVE_SSE41) && !defined(FE_QGEMM_HAVE_AVX2)
    const __m128 vinv = _mm_set1_ps(inv);
    const __m128 vzof = _mm_set1_ps(zof);
    for (; i + 3 < n; i += 4) {
        __m128 v = _mm_add_ps(_mm_mul_ps(fe_fp16_load4_soft(p + i), vinv), vzof);
        __m128i q = _mm_cvtps_epi32(v);
        __m128i s = _mm_packs_epi32(q, q);
        __m128i b = _mm_packs_epi16(s, s);
        int32_t w = _mm_cvtsi128_si32(b);
        for (int k = 0; k < 4; ++k) out[i + k] = (int8_t)((w >> (k * 8)) & 0xff);
    }
#else
    const __m256 vinv = _mm256_set1_ps(inv);
    const __m256 vzof = _mm256_set1_ps(zof);
    for (; i + 7 < n; i += 8) {
        __m256 v = _mm256_fmadd_ps(fe_fp16_load8(p + i), vinv, vzof);
        __m256i q = _mm256_cvtps_epi32(v);
        __m128i lo = _mm256_castsi256_si128(q);
        __m128i hi = _mm256_extracti128_si256(q, 1);
        __m128i s  = _mm_packs_epi32(lo, hi);
        __m128i b  = _mm_packs_epi16(s, s);
        _mm_storel_epi64((__m128i *)(out + i), b);
    }
#endif
#elif defined(__aarch64__) || defined(__arm64__) || defined(_M_ARM64)
    const float32x4_t vinv = vdupq_n_f32(inv);
    const float32x4_t vzof = vdupq_n_f32(zof);
    for (; i + 15 < n; i += 16) {
        float32x4_t v0 = fe_fp16_load4(p + i +  0);
        float32x4_t v1 = fe_fp16_load4(p + i +  4);
        float32x4_t v2 = fe_fp16_load4(p + i +  8);
        float32x4_t v3 = fe_fp16_load4(p + i + 12);
        int32x4_t q0 = vcvtnq_s32_f32(vfmaq_f32(vzof, v0, vinv));
        int32x4_t q1 = vcvtnq_s32_f32(vfmaq_f32(vzof, v1, vinv));
        int32x4_t q2 = vcvtnq_s32_f32(vfmaq_f32(vzof, v2, vinv));
        int32x4_t q3 = vcvtnq_s32_f32(vfmaq_f32(vzof, v3, vinv));
        int16x8_t s01 = vcombine_s16(vqmovn_s32(q0), vqmovn_s32(q1));
        int16x8_t s23 = vcombine_s16(vqmovn_s32(q2), vqmovn_s32(q3));
        vst1q_s8(out + i, vcombine_s8(vqmovn_s16(s01), vqmovn_s16(s23)));
    }
    for (; i + 3 < n; i += 4) {
        float32x4_t v = fe_fp16_load4(p + i);
        int32x4_t q = vcvtnq_s32_f32(vfmaq_f32(vzof, v, vinv));
        int16x4_t s = vqmovn_s32(q);
        int8x8_t  b = vqmovn_s16(vcombine_s16(s, vdup_n_s16(0)));
        uint32_t  w = (uint32_t)vget_lane_u32(vreinterpret_u32_s8(b), 0);
        for (int k = 0; k < 4; ++k) out[i + k] = (int8_t)((w >> (k * 8)) & 0xff);
    }
#endif
    if (i != n) fe_fp16_alignment_violation();
}

#endif /* FE_FP16_H */
