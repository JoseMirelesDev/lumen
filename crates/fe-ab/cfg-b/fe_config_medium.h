/*
 * fe_config_medium.h -- FastEnhancer-Base 48 kHz compile-time constants.
 *
 * Mirrors the upstream medium header with the Base dims. Source:
 * configs/fastenhancer_48khz/b.yaml + onnx-48khz-v1 release:
 *   channels (C1)            = 48
 *   ENC_BLOCKS               = 2    (kernel_size [8, 3, 3])
 *   rnnformer.channels (C2)  = 36   -> PADDED to 40 (see below)
 *   rnnformer.freq    (F2)   = 36   -> PADDED to 40 (see below)
 *   RF_BLOCKS                = 3
 *   num_heads                = 4    (head_dim = 9 real, 10 padded)
 *   n_fft                    = 1024
 *   hop_size                 = 512  (10.67 ms @ 48 kHz)
 *   win_size                 = 1024
 *   input_compression        = 0.3
 *
 * SIMD PADDING: the released Base model has C2=F2=36, which is NOT a
 * multiple of the 8x8 GEMM tile, so every RNNFormer GEMM/GRU fell to the
 * scalar tail (~12-22x slower per MAC than the AVX2 kernels) and Base ran
 * SLOWER than Medium. The fe-ab experiment build instead pads the
 * RNNFormer dims to 40 (weights zero-padded in the q8 blob; padded
 * activations stay exactly zero by construction) so all GEMMs/GRUs are
 * 8-aligned and run SIMD. Math neutrality:
 *   - padded weight rows (N dim) are 0 with bias 0 -> output col = 0
 *   - padded weight cols (K dim) multiply activation 0 -> contribution 0
 *   - attention scale uses the REAL head dim (FE_HEAD_DIM_REAL) so the
 *     1/sqrt(HD) temperature is unchanged
 *   - softmax sums only the REAL key columns (FE_F2_REAL); the padded
 *     keys have zero logits and must not add exp(0) to the row sum
 *   - padded GRU gates have zero W/bias -> h[padded] stays exactly 0
 */
#ifndef FE_CONFIG_MEDIUM_H
#define FE_CONFIG_MEDIUM_H

/* hop 512 @ 48 kHz = 10.67 ms per fe_run() call (onnx-48khz-v1 b.yaml).
 * Must be defined BEFORE fe.h (whose #ifndef default is 320) so the STFT
 * framing matches the released model. */
#define FE_FRAME_SIZE    512

#include "fe.h"   /* FE_SAMPLE_RATE, FE_FRAME_SIZE (public ABI constants) */

/* Audio */
#define FE_N_FFT         1024
#define FE_HOP_SIZE      FE_FRAME_SIZE  /* 512, 10.67 ms */
#define FE_WIN_SIZE      1024
#define FE_FREQ_BINS     (FE_N_FFT / 2)         /* 512 */
#define FE_SPEC_BINS     (FE_N_FFT / 2 + 1)     /* 513 */
#define FE_CACHE_LEN     (FE_N_FFT - FE_HOP_SIZE) /* 512 */

/* Compression (input/output dynamic range) */
#define FE_COMPRESS_EXP  0.3f
#define FE_COMPRESS_IN   (FE_COMPRESS_EXP - 1.0f)        /* -0.7  */
#define FE_COMPRESS_OUT  (1.0f / FE_COMPRESS_EXP - 1.0f) /*  2.333 */

/* Encoder / Decoder */
#define FE_STRIDE        4
#define FE_ENC_K0        8     /* enc_pre kernel */
#define FE_ENC_K         3     /* encoder block kernel */
#define FE_C1            48    /* encoder/decoder channels */
#define FE_ENC_BLOCKS    2
#define FE_DEC_BLOCKS    FE_ENC_BLOCKS

/* RNNFormer (PADDED dims; real model values in *_REAL) */
#define FE_C2            40    /* rf channels (real 36, padded) */
#define FE_F2            40    /* rf freq    (real 36, padded) */
#define FE_RF_BLOCKS     3
#define FE_NUM_HEADS     4
#define FE_HEAD_DIM      (FE_C2 / FE_NUM_HEADS)  /* 10 (real 9) */
#define FE_HEAD_DIM_REAL 9     /* attention 1/sqrt(HD) temperature */
#define FE_F2_REAL       36    /* softmax sums only these key columns */

#define FE_HEAD_DIM_PAD_K 16   /* 8-aligned: qk GEMM N=16 runs SIMD */
#define FE_HEAD_DIM_PAD_V 16

#define FE_GRU_DIM       FE_C2
#define FE_GRU_GATES     3

/* Derived */
#define FE_F1            (FE_FREQ_BINS / FE_STRIDE) /* 128 */

/* StridedConv reshape (input [2, F=512] -> [8, F1+1=129], Conv1d(8->C1, k=2)) */
#define FE_STRIDED_CI    (FE_STRIDE * 2)         /* 8   */
#define FE_STRIDED_FNEW  (FE_F1 + 1)             /* 129 */

/* All GEMM M/N dims are 8-aligned with the padded config (40, 120, 128,
 * 48, 16), so the SIMD kernels run with no scalar tail. */
_Static_assert(FE_C1            % 8 == 0, "FE_C1 must be a multiple of the 8x8 GEMM tile");
_Static_assert(FE_C2            % 8 == 0, "FE_C2 must be a multiple of the 8x8 GEMM tile");
_Static_assert(FE_F2            % 8 == 0, "FE_F2 must be a multiple of the 8x8 GEMM tile");
_Static_assert(FE_F1            % 8 == 0, "FE_F1 must be a multiple of the 8x8 GEMM tile");
_Static_assert(FE_HEAD_DIM_PAD_K % 8 == 0, "FE_HEAD_DIM_PAD_K must be a multiple of the 8x8 GEMM tile");
_Static_assert(FE_HEAD_DIM_PAD_V % 8 == 0, "FE_HEAD_DIM_PAD_V must be a multiple of the 8x8 GEMM tile");

#endif /* FE_CONFIG_MEDIUM_H */
