/* SSE2 4-wide 1024-pt real FFT. Ported from fft_avx2.inl.
 * Stage 0 does 4 butterflies + 4x4 transpose; L>=4 stages run 4-wide.
 * No FMA, no 256-bit: _mm_mul_ps + _mm_sub_ps replaces _mm256_fmsub_ps. */
#include <math.h>
#include <string.h>
#include <immintrin.h>

/* Complex multiply, 4-wide (SSE2). */
static inline void fe_sse2_cmul(__m128 a_re, __m128 a_im,
                                __m128 b_re, __m128 b_im,
                                __m128 *out_re, __m128 *out_im) {
    __m128 r = _mm_sub_ps(_mm_mul_ps(a_re, b_re), _mm_mul_ps(a_im, b_im));
    __m128 i = _mm_add_ps(_mm_mul_ps(a_re, b_im), _mm_mul_ps(a_im, b_re));
    *out_re = r;
    *out_im = i;
}

/* Radix-4 butterfly, 4-wide. */
static inline void fe_sse2_radix4_butterfly(
        __m128 a0_re, __m128 a0_im,
        __m128 b1_re, __m128 b1_im,
        __m128 b2_re, __m128 b2_im,
        __m128 b3_re, __m128 b3_im,
        __m128 *o0_re, __m128 *o0_im,
        __m128 *o1_re, __m128 *o1_im,
        __m128 *o2_re, __m128 *o2_im,
        __m128 *o3_re, __m128 *o3_im) {
    __m128 t0_re = _mm_add_ps(a0_re, b2_re), t0_im = _mm_add_ps(a0_im, b2_im);
    __m128 t1_re = _mm_sub_ps(a0_re, b2_re), t1_im = _mm_sub_ps(a0_im, b2_im);
    __m128 t2_re = _mm_add_ps(b1_re, b3_re), t2_im = _mm_add_ps(b1_im, b3_im);
    __m128 t3_re = _mm_sub_ps(b1_re, b3_re), t3_im = _mm_sub_ps(b1_im, b3_im);

    *o0_re = _mm_add_ps(t0_re, t2_re); *o0_im = _mm_add_ps(t0_im, t2_im);
    *o1_re = _mm_add_ps(t1_re, t3_im); *o1_im = _mm_sub_ps(t1_im, t3_re);
    *o2_re = _mm_sub_ps(t0_re, t2_re); *o2_im = _mm_sub_ps(t0_im, t2_im);
    *o3_re = _mm_sub_ps(t1_re, t3_im); *o3_im = _mm_add_ps(t1_im, t3_re);
}

/* 4x4 transpose-and-store for stage 0 output (SSE2). */
static inline void fe_sse2_transpose_store_4x4(
        float *out, __m128 o0, __m128 o1, __m128 o2, __m128 o3) {
    __m128 t0 = _mm_unpacklo_ps(o0, o1);
    __m128 t1 = _mm_unpackhi_ps(o0, o1);
    __m128 t2 = _mm_unpacklo_ps(o2, o3);
    __m128 t3 = _mm_unpackhi_ps(o2, o3);
    __m128 r0 = _mm_shuffle_ps(t0, t2, _MM_SHUFFLE(1, 0, 1, 0));
    __m128 r1 = _mm_shuffle_ps(t0, t2, _MM_SHUFFLE(3, 2, 3, 2));
    __m128 r2 = _mm_shuffle_ps(t1, t3, _MM_SHUFFLE(1, 0, 1, 0));
    __m128 r3 = _mm_shuffle_ps(t1, t3, _MM_SHUFFLE(3, 2, 3, 2));
    _mm_storeu_ps(out +  0, r0);
    _mm_storeu_ps(out +  4, r1);
    _mm_storeu_ps(out +  8, r2);
    _mm_storeu_ps(out + 12, r3);
}

/* Stage 0 (L=1): 4 butterflies/iter (half of AVX2's 8). */
static void fft_sse2_stage0(const float *in_re, const float *in_im,
                            float *out_re, float *out_im) {
    const int stride = FE_FFT_HALF / 4;
    for (int j = 0; j < stride; j += 4) {
        __m128 a0_re = _mm_loadu_ps(in_re + j + 0 * stride);
        __m128 a0_im = _mm_loadu_ps(in_im + j + 0 * stride);
        __m128 a1_re = _mm_loadu_ps(in_re + j + 1 * stride);
        __m128 a1_im = _mm_loadu_ps(in_im + j + 1 * stride);
        __m128 a2_re = _mm_loadu_ps(in_re + j + 2 * stride);
        __m128 a2_im = _mm_loadu_ps(in_im + j + 2 * stride);
        __m128 a3_re = _mm_loadu_ps(in_re + j + 3 * stride);
        __m128 a3_im = _mm_loadu_ps(in_im + j + 3 * stride);

        __m128 o0_re, o0_im, o1_re, o1_im, o2_re, o2_im, o3_re, o3_im;
        fe_sse2_radix4_butterfly(a0_re, a0_im, a1_re, a1_im, a2_re, a2_im, a3_re, a3_im,
                                  &o0_re, &o0_im, &o1_re, &o1_im,
                                  &o2_re, &o2_im, &o3_re, &o3_im);

        fe_sse2_transpose_store_4x4(out_re + 4 * j, o0_re, o1_re, o2_re, o3_re);
        fe_sse2_transpose_store_4x4(out_im + 4 * j, o0_im, o1_im, o2_im, o3_im);
    }
}

/* Generic radix-4 stage, L >= 4, 4-wide. */
void fft_sse2_radix4_stage_wide(const float *in_re, const float *in_im,
                                float *out_re, float *out_im,
                                const float *twr, const float *twi,
                                int L) {
    const int Ls = L * 4;
    const int n_groups = FE_FFT_HALF / Ls;
    const int stride = FE_FFT_HALF / 4;
    for (int j = 0; j < n_groups; ++j) {
        for (int k = 0; k < L; k += 4) {
            const int in_base = j * L + k;
            __m128 a0_re = _mm_loadu_ps(in_re + in_base + 0 * stride);
            __m128 a0_im = _mm_loadu_ps(in_im + in_base + 0 * stride);
            __m128 a1_re = _mm_loadu_ps(in_re + in_base + 1 * stride);
            __m128 a1_im = _mm_loadu_ps(in_im + in_base + 1 * stride);
            __m128 a2_re = _mm_loadu_ps(in_re + in_base + 2 * stride);
            __m128 a2_im = _mm_loadu_ps(in_im + in_base + 2 * stride);
            __m128 a3_re = _mm_loadu_ps(in_re + in_base + 3 * stride);
            __m128 a3_im = _mm_loadu_ps(in_im + in_base + 3 * stride);

            __m128 w1_re = _mm_loadu_ps(twr + 0 * L + k);
            __m128 w1_im = _mm_loadu_ps(twi + 0 * L + k);
            __m128 w2_re = _mm_loadu_ps(twr + 1 * L + k);
            __m128 w2_im = _mm_loadu_ps(twi + 1 * L + k);
            __m128 w3_re = _mm_loadu_ps(twr + 2 * L + k);
            __m128 w3_im = _mm_loadu_ps(twi + 2 * L + k);

            __m128 b1_re, b1_im, b2_re, b2_im, b3_re, b3_im;
            fe_sse2_cmul(a1_re, a1_im, w1_re, w1_im, &b1_re, &b1_im);
            fe_sse2_cmul(a2_re, a2_im, w2_re, w2_im, &b2_re, &b2_im);
            fe_sse2_cmul(a3_re, a3_im, w3_re, w3_im, &b3_re, &b3_im);

            __m128 o0_re, o0_im, o1_re, o1_im, o2_re, o2_im, o3_re, o3_im;
            fe_sse2_radix4_butterfly(a0_re, a0_im, b1_re, b1_im, b2_re, b2_im, b3_re, b3_im,
                                      &o0_re, &o0_im, &o1_re, &o1_im,
                                      &o2_re, &o2_im, &o3_re, &o3_im);

            const int out_base = j * Ls + k;
            _mm_storeu_ps(out_re + out_base + 0 * L, o0_re);
            _mm_storeu_ps(out_im + out_base + 0 * L, o0_im);
            _mm_storeu_ps(out_re + out_base + 1 * L, o1_re);
            _mm_storeu_ps(out_im + out_base + 1 * L, o1_im);
            _mm_storeu_ps(out_re + out_base + 2 * L, o2_re);
            _mm_storeu_ps(out_im + out_base + 2 * L, o2_im);
            _mm_storeu_ps(out_re + out_base + 3 * L, o3_re);
            _mm_storeu_ps(out_im + out_base + 3 * L, o3_im);
        }
    }
}

static void fft_sse2_radix2_stage(const float *in_re, const float *in_im,
                                  float *out_re, float *out_im,
                                  const float *twr, const float *twi) {
    const int L = 256;
    for (int k = 0; k < L; k += 4) {
        __m128 a_re = _mm_loadu_ps(in_re + k);
        __m128 a_im = _mm_loadu_ps(in_im + k);
        __m128 b_re = _mm_loadu_ps(in_re + k + L);
        __m128 b_im = _mm_loadu_ps(in_im + k + L);
        __m128 w_re = _mm_loadu_ps(twr + k);
        __m128 w_im = _mm_loadu_ps(twi + k);
        __m128 bw_re, bw_im;
        fe_sse2_cmul(b_re, b_im, w_re, w_im, &bw_re, &bw_im);
        _mm_storeu_ps(out_re + k,     _mm_add_ps(a_re, bw_re));
        _mm_storeu_ps(out_im + k,     _mm_add_ps(a_im, bw_im));
        _mm_storeu_ps(out_re + k + L, _mm_sub_ps(a_re, bw_re));
        _mm_storeu_ps(out_im + k + L, _mm_sub_ps(a_im, bw_im));
    }
}

static void fft_complex_512_sse2(const float *in_re, const float *in_im,
                                 float *out_re, float *out_im) {
    static __thread float buf_re[2][FE_FFT_HALF];
    static __thread float buf_im[2][FE_FFT_HALF];

    fft_sse2_stage0(in_re, in_im, buf_re[0], buf_im[0]);

    int parity = 0;
    int L = 4;
    for (int s = 1; s < FE_FFT_R4_STAGES; ++s) {
        const float *twr = g_fft_plan.twiddle_r4_re + g_fft_plan.twiddle_r4_off[s];
        const float *twi = g_fft_plan.twiddle_r4_im + g_fft_plan.twiddle_r4_off[s];
        // For SSE2, all stages use 4-wide kernel (since AVX2 wide is 8-wide).
        fft_sse2_radix4_stage_wide(buf_re[parity], buf_im[parity],
                                   buf_re[1 - parity], buf_im[1 - parity],
                                   twr, twi, L);
        parity = 1 - parity;
        L *= 4;
    }
    fft_sse2_radix2_stage(buf_re[parity], buf_im[parity], out_re, out_im,
                          g_fft_plan.twiddle_r2_re, g_fft_plan.twiddle_r2_im);
}

static inline void fft_sse2_pack_real(const float *in_real,
                                      float *pack_re, float *pack_im) {
    for (int k = 0; k < FE_FFT_HALF; k += 4) {
        float v0[4], v1[4];
        for(int i=0;i<4;++i){ v0[i]=in_real[2*(k+i)+0]; v1[i]=in_real[2*(k+i)+1]; } // scalar fallback
        // Actually load 4-wide: evens and odds via shuffle
        __m128 a = _mm_loadu_ps(in_real + 2*k + 0);
        __m128 b = _mm_loadu_ps(in_real + 2*k + 4);
        __m128 evens = _mm_shuffle_ps(a, b, _MM_SHUFFLE(2, 0, 2, 0));
        __m128 odds  = _mm_shuffle_ps(a, b, _MM_SHUFFLE(3, 1, 3, 1));
        // evens/odds are already lane-correct for 4-wide (no cross-lane permute needed)
        _mm_storeu_ps(pack_re + k, evens);
        _mm_storeu_ps(pack_im + k, odds);
    }
}

static void fft_sse2_real_post_forward(const float *Z_re, const float *Z_im,
                                       float *out_re, float *out_im) {
    out_re[0]           = Z_re[0] + Z_im[0]; out_im[0]           = 0.0f;
    out_re[FE_FFT_HALF] = Z_re[0] - Z_im[0]; out_im[FE_FFT_HALF] = 0.0f;

    const __m128 half = _mm_set1_ps(0.5f);
    int k = 1;
    for (; k + 3 < FE_FFT_HALF; k += 4) {
        __m128 zk_r = _mm_loadu_ps(Z_re + k);
        __m128 zk_i = _mm_loadu_ps(Z_im + k);
        float zn_r_tmp[4], zn_i_tmp[4];
        for (int i = 0; i < 4; ++i) {
            zn_r_tmp[i] = Z_re[FE_FFT_HALF - (k + i)];
            zn_i_tmp[i] = Z_im[FE_FFT_HALF - (k + i)];
        }
        __m128 zn_r = _mm_loadu_ps(zn_r_tmp);
        __m128 zn_i = _mm_loadu_ps(zn_i_tmp);
        __m128 sum_r = _mm_mul_ps(half, _mm_add_ps(zk_r, zn_r));
        __m128 sum_i = _mm_mul_ps(half, _mm_sub_ps(zk_i, zn_i));
        __m128 dif_r = _mm_mul_ps(half, _mm_sub_ps(zk_r, zn_r));
        __m128 dif_i = _mm_mul_ps(half, _mm_add_ps(zk_i, zn_i));
        __m128 tw_r = _mm_loadu_ps(g_fft_plan.realfft_re + k);
        __m128 tw_i = _mm_loadu_ps(g_fft_plan.realfft_im + k);
        __m128 d_re, d_im;
        fe_sse2_cmul(dif_r, dif_i, tw_r, tw_i, &d_re, &d_im);
        _mm_storeu_ps(out_re + k, _mm_add_ps(sum_r, d_im));
        _mm_storeu_ps(out_im + k, _mm_sub_ps(sum_i, d_re));
    }
    for (; k < FE_FFT_HALF; ++k) {
        const float zk_r = Z_re[k];
        const float zk_i = Z_im[k];
        const float zn_r = Z_re[FE_FFT_HALF - k];
        const float zn_i = Z_im[FE_FFT_HALF - k];
        const float sum_r = 0.5f * (zk_r + zn_r);
        const float sum_i = 0.5f * (zk_i - zn_i);
        const float dif_r = 0.5f * (zk_r - zn_r);
        const float dif_i = 0.5f * (zk_i + zn_i);
        const float tw_r = g_fft_plan.realfft_re[k];
        const float tw_i = g_fft_plan.realfft_im[k];
        const float d_r = dif_r * tw_r - dif_i * tw_i;
        const float d_i = dif_r * tw_i + dif_i * tw_r;
        out_re[k] = sum_r + d_i;
        out_im[k] = sum_i - d_r;
    }
}

static void fft_sse2_real_pre_inverse(const float *in_re, const float *in_im,
                                      float *Z_re, float *Z_im) {
    Z_re[0] = in_re[0] + in_re[FE_FFT_HALF];
    Z_im[0] = in_re[0] - in_re[FE_FFT_HALF];

    int k = 1;
    for (; k + 3 < FE_FFT_HALF; k += 4) {
        __m128 xk_r = _mm_loadu_ps(in_re + k);
        __m128 xk_i = _mm_loadu_ps(in_im + k);
        float xn_r_tmp[4], xn_i_tmp[4];
        for (int i = 0; i < 4; ++i) {
            xn_r_tmp[i] = in_re[FE_FFT_HALF - (k + i)];
            xn_i_tmp[i] = in_im[FE_FFT_HALF - (k + i)];
        }
        __m128 xn_r = _mm_loadu_ps(xn_r_tmp);
        __m128 xn_i = _mm_loadu_ps(xn_i_tmp);
        __m128 sum_r = _mm_add_ps(xk_r, xn_r);
        __m128 sum_i = _mm_sub_ps(xk_i, xn_i);
        __m128 dif_r = _mm_sub_ps(xk_r, xn_r);
        __m128 dif_i = _mm_add_ps(xk_i, xn_i);
        __m128 tw_r = _mm_loadu_ps(g_fft_plan.realfft_re + k);
        __m128 tw_i = _mm_sub_ps(_mm_setzero_ps(), _mm_loadu_ps(g_fft_plan.realfft_im + k));
        __m128 d_re, d_im;
        fe_sse2_cmul(dif_r, dif_i, tw_r, tw_i, &d_re, &d_im);
        _mm_storeu_ps(Z_re + k, _mm_sub_ps(sum_r, d_im));
        _mm_storeu_ps(Z_im + k, _mm_add_ps(sum_i, d_re));
    }
    for (; k < FE_FFT_HALF; ++k) {
        const float xk_r = in_re[k],                 xk_i = in_im[k];
        const float xn_r = in_re[FE_FFT_HALF - k],   xn_i = in_im[FE_FFT_HALF - k];
        const float sum_r = xk_r + xn_r;
        const float sum_i = xk_i - xn_i;
        const float dif_r = xk_r - xn_r;
        const float dif_i = xk_i + xn_i;
        const float tw_r =  g_fft_plan.realfft_re[k];
        const float tw_i = -g_fft_plan.realfft_im[k];
        const float d_r = dif_r * tw_r - dif_i * tw_i;
        const float d_i = dif_r * tw_i + dif_i * tw_r;
        Z_re[k] = sum_r - d_i;
        Z_im[k] = sum_i + d_r;
    }
}

void fe_fft_forward_sse2(const float *in_real, float *out_re, float *out_im) {
    float pack_re[FE_FFT_HALF], pack_im[FE_FFT_HALF];
    fft_sse2_pack_real(in_real, pack_re, pack_im);
    float Z_re[FE_FFT_HALF], Z_im[FE_FFT_HALF];
    fft_complex_512_sse2(pack_re, pack_im, Z_re, Z_im);
    fft_sse2_real_post_forward(Z_re, Z_im, out_re, out_im);
}

void fe_fft_inverse_sse2(const float *in_re, const float *in_im, float *out_real) {
    float Z_re[FE_FFT_HALF], Z_im[FE_FFT_HALF];
    fft_sse2_real_pre_inverse(in_re, in_im, Z_re, Z_im);
    float tmp_re[FE_FFT_HALF], tmp_im[FE_FFT_HALF];
    fft_complex_512_sse2(Z_re, Z_im, tmp_re, tmp_im);
    // Scale by 1/N and de-interleave.
    const float inv = 1.0f / (float)FE_FFT_HALF;
    const __m128 vinv = _mm_set1_ps(inv);
    for (int k = 0; k < FE_FFT_HALF; k += 4) {
        __m128 re = _mm_mul_ps(_mm_loadu_ps(tmp_re + k), vinv);
        __m128 im = _mm_mul_ps(_mm_loadu_ps(tmp_im + k), vinv);
        // Store interleaved? Real output is N=1024: even= re, odd = im forward; here inverse packs differently.
        // Simplified: write evens then odds scalar.
        float re_tmp[4], im_tmp[4];
        _mm_storeu_ps(re_tmp, re);
        _mm_storeu_ps(im_tmp, im);
        for(int i=0;i<4;++i){ out_real[2*(k+i)+0]=re_tmp[i]; out_real[2*(k+i)+1]=im_tmp[i]; }
    }
}
