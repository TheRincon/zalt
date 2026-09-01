/*
 * zalt — Zstandard decoding for Apple Silicon.
 *
 * C ABI for in-process embedding from C, C++, Objective-C and Swift.
 *
 * Hand-written rather than generated. A generated header follows whatever the
 * Rust happens to look like; a hand-written one *is* the contract, and the
 * Rust has to match it. `ci/check-abi.sh` enforces that the two agree.
 *
 * Threading
 *   A zalt_decoder belongs to one thread at a time. The library starts no
 *   threads and holds no global state, so a host is free to use its own
 *   threading model without arranging anything around ours.
 *
 * Memory
 *   No allocation crosses this boundary. Output goes into caller-provided
 *   buffers, so the host never calls back to free anything and can account for
 *   every byte. zalt_decoder_footprint reports what a decoder holds.
 *
 * Failure
 *   No panic escapes: every entry point converts one into ZALT_ERR_PANIC
 *   rather than unwinding into your stack or aborting your process. A
 *   malformed subresource fails that resource, not the host.
 */

#ifndef ZALT_H
#define ZALT_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ---------------------------------------------------------------- version */

/* Bumped on any incompatible change below. Check it at load time. */
#define ZALT_ABI_VERSION 1u

uint32_t zalt_abi_version(void);

/* Statically allocated; do not free. */
const char *zalt_version_string(void);

/* "neon" or "scalar". Statically allocated. */
const char *zalt_backend_name(void);

/* ----------------------------------------------------------------- status */

/* Returned in the `status` out-parameter of zalt_decoder_decompress. */
#define ZALT_STATUS_NEED_MORE_INPUT 0
#define ZALT_STATUS_HAS_MORE_OUTPUT 1
#define ZALT_STATUS_FRAME_COMPLETE 2

/* ----------------------------------------------------------------- errors */

/*
 * Return codes. Zero is success; everything else is negative.
 *
 * Codes at or below -100 come from the codec and encode a specific format
 * failure. They are assigned explicitly rather than by declaration order, so
 * reordering anything in the Rust cannot silently change what you read here.
 */
#define ZALT_OK 0
#define ZALT_ERR_NULL_POINTER (-1)
#define ZALT_ERR_OUTPUT_TOO_SMALL (-2)
#define ZALT_ERR_OVER_BUDGET (-3)
#define ZALT_ERR_PANIC (-4)
#define ZALT_ERR_BAD_ARGUMENT (-5)

/* Statically allocated; do not free. Never NULL. */
const char *zalt_error_message(int32_t code);

/*
 * Whether acting on the error might succeed where failing the resource would
 * not: fetching a dictionary, retrying with a larger buffer, or re-requesting
 * without `zstd` in Accept-Encoding.
 */
bool zalt_error_is_recoverable(int32_t code);

/* ---------------------------------------------------------------- decoder */

typedef struct zalt_decoder zalt_decoder;

/*
 * Create a decoder.
 *
 *   max_window_log     Largest window a frame may declare, 10..=31. Pass 23
 *                      for the 8 MiB ceiling appropriate to untrusted network
 *                      input, or 0 for the library default.
 *   max_decoded_size   Ceiling on total output, or 0 for none.
 *
 * Returns NULL on allocation failure or an out-of-range argument.
 */
zalt_decoder *zalt_decoder_new(uint8_t max_window_log, uint64_t max_decoded_size);

/* Destroy a decoder. NULL is allowed and does nothing. */
void zalt_decoder_free(zalt_decoder *handle);

/* Re-arm for an unrelated stream, keeping allocations. Cheap; prefer it to
 * destroying and recreating a decoder per response. */
int32_t zalt_decoder_reset(zalt_decoder *handle);

/* Release the decoder's buffers, keeping it usable. Use when a decoder has
 * grown serving one large response and should not hold that memory. */
int32_t zalt_decoder_shrink(zalt_decoder *handle);

/* Bytes of heap the decoder holds. Returns 0 for NULL. */
size_t zalt_decoder_footprint(const zalt_decoder *handle);

/*
 * Decode incrementally.
 *
 * Consumes from `input`, writes to `output`, and reports what it needs next.
 * Suspends at any byte boundary, so socket-sized chunks are fine and the
 * output buffer may be any size — including one byte.
 *
 * Out-parameters may be NULL if you do not need them.
 *
 * A frame reports ZALT_STATUS_FRAME_COMPLETE only once all of its output has
 * been delivered. If the output buffer fills first you get
 * ZALT_STATUS_HAS_MORE_OUTPUT; call again with space and no new input.
 *
 * Returns ZALT_OK or a negative error code.
 *
 *   size_t consumed, written; int32_t status;
 *   int32_t rc = zalt_decoder_decompress(
 *       dec, in + off, in_len - off, &consumed,
 *       out, sizeof out, &written, &status);
 *   if (rc != ZALT_OK) { ... }
 *   off += consumed;
 */
int32_t zalt_decoder_decompress(zalt_decoder *handle,
                                const uint8_t *input,
                                size_t input_len,
                                size_t *input_consumed,
                                uint8_t *output,
                                size_t output_cap,
                                size_t *output_written,
                                int32_t *status);

/* --------------------------------------------------------------- one-shot */

/*
 * Decode a complete frame into a caller-provided buffer.
 *
 * Returns ZALT_ERR_OUTPUT_TOO_SMALL when output_cap is insufficient; use
 * zalt_frame_content_size first when the frame declares its size.
 */
int32_t zalt_decompress(const uint8_t *input,
                        size_t input_len,
                        uint8_t max_window_log,
                        uint8_t *output,
                        size_t output_cap,
                        size_t *output_written);

/*
 * Read a frame's declared content size without decoding it.
 *
 * Returns ZALT_ERR_BAD_ARGUMENT when the frame declares no size, which is
 * legal — a streaming encoder need not know it in advance. Size the output
 * buffer some other way in that case.
 */
int32_t zalt_frame_content_size(const uint8_t *input, size_t input_len, uint64_t *out);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* ZALT_H */
