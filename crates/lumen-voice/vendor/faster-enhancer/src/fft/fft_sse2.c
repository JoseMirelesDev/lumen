/* x86 SSE2 FFT TU. Compiled with `-msse2` (baseline x86_64).
 * Fallback for Pentium/Celeron without AVX2. 4-wide SSE2 ops, no FMA.
 */
#if defined(__x86_64__) || defined(_M_X64) || defined(__i386__) || defined(_M_IX86)
#include "fft_arch.h"
#include <immintrin.h>

/* Pull in the SSE2 inline kernels. g_fft_plan is extern from fe_fft.c. */
#include "fft_sse2.inl"

#endif /* x86 */
