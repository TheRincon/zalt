/* Profile the *reference* decoder in the same shape as `zalt-bench profile`.
 *
 * Build and run it with `just profile-zstd <file.zst>`; this file is not part
 * of the Rust workspace and nothing links it.
 *
 * # Why this exists
 *
 * "Where is zstd faster than us" was answered three times by inference and
 * wrongly each time — bounds checks, double-symbol Huffman, prefetching. All
 * three were plausible, none survived a measurement (see doc/ZALT-MAC.md §10,
 * ninth and tenth passes). Comparing two *throughput numbers* cannot answer it,
 * because a ratio says how much slower and never which part.
 *
 * So: profile the reference the same way we profile ourselves, and compare
 * leaf-weighted profiles category by category. That found the answer in one
 * run — the gap is 3.1x, entirely in the fused sequence-decode-and-execute
 * loop, while entropy table setup is the area where zalt is *closest*.
 *
 * # Why a C harness rather than timing the zstd CLI
 *
 * The shapes have to match or the comparison measures the harness. `zalt-bench
 * profile` decodes from a buffer already in memory, through a context it reuses
 * across iterations, into a preallocated output buffer, with no file I/O and no
 * allocation inside the loop. This does exactly that. `zstd -t` would add a
 * process spawn and a file read per iteration — about 4.83 ms against a 52.7 ms
 * run, which is how the reference came to be understated for a whole milestone.
 *
 * The check that it is honest: it reproduces `zstd -b3` to within 3% on both
 * corpora. If it ever stops doing that, this file is wrong, not zstd.
 *
 * # Reading the output
 *
 * libzstd inlines the FSE state machine, the bit reader and ZSTD_execSequence
 * into one function, so `ZSTD_decompressSequences` covers what zalt splits
 * across `sequences::decode`, `BackwardBits`, `execute` and the copy kernels.
 * Compare that *sum* against it; a leaf profile cannot decompose the reference
 * further, which is a real limit on the resolution of the comparison.
 */
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>
#include <zstd.h>

static double now_sec(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: zstd-profile <file.zst> [seconds]\n");
        return 2;
    }
    const double seconds = (argc > 2) ? atof(argv[2]) : 14.0;

    FILE *f = fopen(argv[1], "rb");
    if (!f) { perror("open"); return 1; }
    if (fseek(f, 0, SEEK_END) != 0) { perror("seek"); return 1; }
    const long insize = ftell(f);
    if (insize <= 0) { fprintf(stderr, "empty input\n"); return 1; }
    rewind(f);

    void *in = malloc((size_t)insize);
    if (!in) { fprintf(stderr, "oom\n"); return 1; }
    if (fread(in, 1, (size_t)insize, f) != (size_t)insize) { perror("read"); return 1; }
    fclose(f);

    /* A declared content size lets the output buffer be allocated once, outside
     * the loop — the whole point of the harness. Frames without one are not
     * worth a streaming path here; the benchmark corpora all declare it. */
    const unsigned long long outsize = ZSTD_getFrameContentSize(in, (size_t)insize);
    if (outsize == ZSTD_CONTENTSIZE_ERROR || outsize == ZSTD_CONTENTSIZE_UNKNOWN) {
        fprintf(stderr, "frame declares no content size; cannot preallocate\n");
        return 1;
    }
    void *out = malloc((size_t)outsize);
    if (!out) { fprintf(stderr, "oom\n"); return 1; }

    ZSTD_DCtx *dctx = ZSTD_createDCtx();
    if (!dctx) { fprintf(stderr, "ZSTD_createDCtx failed\n"); return 1; }

    /* Verify once, before timing, so a failure is reported once rather than per
     * iteration and buried under the profiler's output. */
    const size_t r = ZSTD_decompressDCtx(dctx, out, (size_t)outsize, in, (size_t)insize);
    if (ZSTD_isError(r)) {
        fprintf(stderr, "%s: %s\n", argv[1], ZSTD_getErrorName(r));
        return 1;
    }

    fprintf(stderr, "profiling %s — %ld -> %llu bytes, libzstd %s, %.0fs\n",
            argv[1], insize, outsize, ZSTD_versionString(), seconds);
    fprintf(stderr, "attach with:  sample %d %.0f\n", (int)getpid(), seconds);

    const double deadline = now_sec() + seconds;
    unsigned long long iters = 0;
    while (now_sec() < deadline) {
        ZSTD_decompressDCtx(dctx, out, (size_t)outsize, in, (size_t)insize);
        iters++;
    }

    const double produced = (double)outsize * (double)iters;
    fprintf(stderr, "%llu iterations, %.0f bytes, ~%.0f MB/s\n",
            iters, produced, produced / seconds / 1e6);

    ZSTD_freeDCtx(dctx);
    free(out);
    free(in);
    return 0;
}
