/*
 * fe_config_medium.h -- FastEnhancer-Medium 48 kHz compile-time constants.
 *
 * Source: configs/fastenhancer_48khz/m.yaml (FastEnhancer reference tree)
 * + the vendored faster-enhancer.c runtime config (src/internal/fe_config_medium.h):
 *   channels (C1)            = 96
 *   ENC_BLOCKS               = 3    (kernel_size [8, 3, 3, 3])
 *   rnnformer.channels (C2)  = 72
 *   rnnformer.freq    (F2)   = 72
 *   RF_BLOCKS                = 4
 *   num_heads                = 4    (head_dim = 18)
 *   n_fft                    = 1024
 *   hop_size                 = 320  (6.67 ms @ 48 kHz)
 *   win_size                 = 1024
 *   input_compression        = 0.3
 *
 * Medium is the runtime's native config: FE_FRAME_SIZE stays at fe.h's
 * default 320, so FE_HOP_SIZE == 320 and the STFT framing is unchanged.
 * All GEMM dims are 8-aligned except HD_PAD_K=20 (attention Q@K^T), which
 * takes the specialised k20 kernels. The fe-ab scalar-tail wrappers are
 * no-ops for Medium (never dispatched).
 */
#ifndef FE_CONFIG_MEDIUM_H
#define FE_CONFIG_MEDIUM_H

#include "fe.h"   /* FE_SAMPLE_RATE, FE_FRAME_SIZE (public ABI constants) */

/* Audio */
#define FE_N_FFT         1024
#define FE_HOP_SIZE      FE_FRAME_SIZE  /* 320, 6.67 ms */
#define FE_WIN_SIZE      1024
#define FE_FREQ_BINS     (FE_N_FFT / 2)         /* 512 */
#define FE_SPEC_BINS     (FE_N_FFT / 2 + 1)     /* 513 */
#define FE_CACHE_LEN     (FE_N_FFT - FE_HOP_SIZE) /* 704 */

/* Compression (input/output dynamic range) */
#define FE_COMPRESS_EXP  0.3f
#define FE_COMPRESS_IN   (FE_COMPRESS_EXP - 1.0f)        /* -0.7  */
#define FE_COMPRESS_OUT  (1.0f / FE_COMPRESS_EXP - 1.0f) /*  2.333 */

/* Encoder / Decoder */
#define FE_STRIDE        4
#define FE_ENC_K0        8     /* enc_pre kernel */
#define FE_ENC_K         3     /* encoder block kernel */
#define FE_C1            96    /* encoder/decoder channels */
#define FE_ENC_BLOCKS    3
#define FE_DEC_BLOCKS    FE_ENC_BLOCKS

/* RNNFormer */
#define FE_C2            72    /* rf channels */
#define FE_F2            72    /* rf freq */
#define FE_RF_BLOCKS     4
#define FE_NUM_HEADS     4
#define FE_HEAD_DIM      (FE_C2 / FE_NUM_HEADS)  /* 18 */

#define FE_HEAD_DIM_REAL 18    /* no-op for Medium (dims not padded) */
#define FE_F2_REAL       72    /* no-op for Medium (dims not padded) */

#define FE_HEAD_DIM_PAD_K  (((FE_HEAD_DIM + 3) / 4) * 4)  /* 20 (k20 kernels) */
#define FE_HEAD_DIM_PAD_V  (((FE_HEAD_DIM + 7) / 8) * 8)  /* 24 */

#define FE_GRU_DIM       FE_C2
#define FE_GRU_GATES     3                       /* r, z, n */

/* Derived */
#define FE_F1            (FE_FREQ_BINS / FE_STRIDE) /* 128 */

/* StridedConv reshape (input [2, F=512] -> [8, F1+1=129], Conv1d(8->C1, k=2)) */
#define FE_STRIDED_CI    (FE_STRIDE * 2)         /* 8   */
#define FE_STRIDED_FNEW  (FE_F1 + 1)             /* 129 */

/* All GEMM M/N dims are 8-aligned (HD_PAD_K=20 takes the k20 kernels). */
_Static_assert(FE_C1            % 8 == 0, "FE_C1 must be a multiple of the 8x8 GEMM tile");
_Static_assert(FE_C2            % 8 == 0, "FE_C2 must be a multiple of the 8x8 GEMM tile");
_Static_assert(FE_F2            % 8 == 0, "FE_F2 must be a multiple of the 8x8 GEMM tile");
_Static_assert(FE_F1            % 8 == 0, "FE_F1 must be a multiple of the 8x8 GEMM tile");
_Static_assert(FE_HEAD_DIM_PAD_V % 8 == 0, "FE_HEAD_DIM_PAD_V must be a multiple of the 8x8 GEMM tile");

#endif /* FE_CONFIG_MEDIUM_H */
