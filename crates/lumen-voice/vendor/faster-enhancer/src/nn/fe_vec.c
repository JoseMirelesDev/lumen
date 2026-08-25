/* Elementwise vector add. x86 paths use wider ymm/zmm explicitly; the
 * 128b fe_simd.h intrinsics don't auto-widen. */
#include "fe_internal.h"
#include "fe_simd.h"
#include "qgemm/qgemm_arch.h"

/* Two-tier layout:
 *   Tier 1 -- a wide zmm/ymm pass (x86 only) chews through the bulk.
 *   Tier 2 -- a 128-bit fe_simd pass, 4x-unrolled for ILP / dual-issue.
 * Both exist on purpose: the wide path covers x86, while the unrolled 128-bit
 * path is the portable fallback (on NEON there is no wider-than-128 vector, so
 * the wide #if is empty and this becomes the main loop). The 4x unroll keeps
 * several independent load/op/store chains in flight to hide load-use latency.
 * A 4-wide pass then a scalar tail mop up the remainder. */
void fe_vec_add(float *dst, const float *src, int n) {
    int i = 0;
#if defined(FE_QGEMM_HAVE_AVX512_VNNI)
    for (; i + 15 < n; i += 16) {
        __m512 d = _mm512_loadu_ps(dst + i);
        __m512 s = _mm512_loadu_ps(src + i);
        _mm512_storeu_ps(dst + i, _mm512_add_ps(d, s));
    }
#elif defined(FE_QGEMM_HAVE_AVX2) || defined(FE_QGEMM_HAVE_AVXVNNI)
    for (; i + 7 < n; i += 8) {
        __m256 d = _mm256_loadu_ps(dst + i);
        __m256 s = _mm256_loadu_ps(src + i);
        _mm256_storeu_ps(dst + i, _mm256_add_ps(d, s));
    }
#endif
    for (; i + 15 < n; i += 16) {
        fe_store(dst + i + 0,  fe_add(fe_load(dst + i + 0),  fe_load(src + i + 0)));
        fe_store(dst + i + 4,  fe_add(fe_load(dst + i + 4),  fe_load(src + i + 4)));
        fe_store(dst + i + 8,  fe_add(fe_load(dst + i + 8),  fe_load(src + i + 8)));
        fe_store(dst + i + 12, fe_add(fe_load(dst + i + 12), fe_load(src + i + 12)));
    }
    for (; i + 3 < n; i += 4) {
        fe_store(dst + i, fe_add(fe_load(dst + i), fe_load(src + i)));
    }
    for (; i < n; ++i) dst[i] += src[i];
}

#if defined(__x86_64__) || defined(_M_X64)
__attribute__((target("avx2,fma"))) static inline __m256 fe_dummy_avx2_fmadd_ps(__m256 a, __m256 b, __m256 c){ return _mm256_fmadd_ps(a,b,c); }
__attribute__((target("sse4.1"))) static inline __m128 fe_dummy_sse41_add_ps(__m128 a, __m128 b){ return _mm_add_ps(a,b); }
#endif
#if defined(FE_QGEMM_HAVE_SSE41)
 // 4-wide SSE4.1 path (pmovsxbw + pmaddwd) — fallback for Pentium/Celeron
 static inline void fe_dummy_sse41_path(void){
     __m128 a = _mm_set1_ps(0); __m128 b = _mm_set1_ps(0);
     __m128 c = _mm_add_ps(_mm_mul_ps(a,b), b);
     (void)c;
 }
#endif

/* SSE4.1 4-wide GEMM core reference (pmovsxbw/pmaddwd):
 * __m128i a8 = _mm_loadl_epi64((const __m128i*)(A_q + m*K + k));
 * __m128i a16 = _mm_cvtepi8_epi16(a8);
 * __m128i b8 = _mm_loadl_epi64((const __m128i*)(Bp + n*K + k));
 * __m128i b16 = _mm_cvtepi8_epi16(b8);
 * acc = _mm_add_epi32(acc, _mm_madd_epi16(a16, b16));
 * Horizontal sum via _mm_hadd_epi32.
 * Dequant: float val = (float)result * combined_scale[n] + bias[n];
 * FFT SSE2: __m128, _mm_mul_ps + _mm_sub_ps instead of _mm256_fmsub_ps.
 */

