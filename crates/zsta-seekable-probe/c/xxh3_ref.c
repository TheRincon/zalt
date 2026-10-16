/* The reference XXH3-128, for zalt-archive's tests: xxHash's own code, all of
 * it inline from the vendored header, in the canonical byte order. */
#include <string.h>
#define XXH_INLINE_ALL
#include "xxhash.h"

void zsta_probe_xxh3_128(const void *data, size_t len, unsigned char out[16]) {
    XXH128_canonical_t c;
    XXH128_canonicalFromHash(&c, XXH3_128bits(data, len));
    memcpy(out, c.digest, 16);
}
