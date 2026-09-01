/*
 * A C caller, compiled and linked against the real staticlib.
 *
 * The Rust ABI tests call the same functions, but from Rust — they prove the
 * logic, not that a C compiler can consume the header or that the symbols
 * actually link. This does. `just ffi-smoke` builds and runs it.
 */

#include <stdio.h>
#include <string.h>

#include "zalt.h"

/* `zstd -1` of "hello world", with a content checksum. */
static const uint8_t HELLO_ZST[] = {
    0x28, 0xb5, 0x2f, 0xfd, 0x24, 0x0b, 0x59, 0x00, 0x00, 0x68, 0x65, 0x6c,
    0x6c, 0x6f, 0x20, 0x77, 0x6f, 0x72, 0x6c, 0x64, 0x68, 0x69, 0x1e, 0xb2,
};

static int failures = 0;

static void check(int condition, const char *what) {
    printf("  %-52s %s\n", what, condition ? "ok" : "FAILED");
    if (!condition) {
        failures++;
    }
}

int main(void) {
    printf("zalt C smoke test\n");
    printf("  version: %s, backend: %s, abi: %u\n\n", zalt_version_string(),
           zalt_backend_name(), zalt_abi_version());

    check(zalt_abi_version() == ZALT_ABI_VERSION, "header and library agree on ABI version");

    /* One-shot. */
    {
        uint8_t out[64];
        size_t written = 0;
        int32_t rc = zalt_decompress(HELLO_ZST, sizeof HELLO_ZST, 23, out,
                                     sizeof out, &written);
        check(rc == ZALT_OK, "one-shot decode returns ZALT_OK");
        check(written == 11 && memcmp(out, "hello world", 11) == 0,
              "one-shot decode produces the right bytes");
    }

    /* Declared content size, without decoding. */
    {
        uint64_t size = 0;
        int32_t rc = zalt_frame_content_size(HELLO_ZST, sizeof HELLO_ZST, &size);
        check(rc == ZALT_OK && size == 11, "content size readable without decoding");
    }

    /* Streaming, one byte of input and one byte of output at a time. This is
     * the shape a host actually drives, and the one where a lost byte hides. */
    {
        zalt_decoder *dec = zalt_decoder_new(23, 0);
        check(dec != NULL, "decoder created");

        char collected[64];
        size_t total = 0;
        size_t cursor = 0;
        int complete = 0;

        for (int guard = 0; guard < 10000 && !complete; guard++) {
            uint8_t byte;
            size_t consumed = 0, written = 0;
            int32_t status = -1;
            size_t avail = (cursor < sizeof HELLO_ZST) ? 1 : 0;

            int32_t rc = zalt_decoder_decompress(dec, HELLO_ZST + cursor, avail,
                                                 &consumed, &byte, 1, &written,
                                                 &status);
            if (rc != ZALT_OK) {
                printf("  decode failed: %s\n", zalt_error_message(rc));
                failures++;
                break;
            }
            cursor += consumed;
            if (written && total < sizeof collected) {
                collected[total++] = (char)byte;
            }
            if (status == ZALT_STATUS_FRAME_COMPLETE) {
                complete = 1;
            }
            if (status == ZALT_STATUS_NEED_MORE_INPUT && avail == 0 && written == 0) {
                break;
            }
        }

        check(complete, "streaming reports frame complete");
        check(total == 11 && memcmp(collected, "hello world", 11) == 0,
              "streaming one byte at a time produces the right bytes");
        check(zalt_decoder_footprint(dec) > 0, "footprint is reportable");
        check(zalt_decoder_shrink(dec) == ZALT_OK, "decoder shrinks");
        check(zalt_decoder_footprint(dec) == 0, "footprint is zero after shrink");
        zalt_decoder_free(dec);
    }

    /* Hostile caller. */
    {
        check(zalt_decoder_reset(NULL) == ZALT_ERR_NULL_POINTER,
              "NULL handle is an error, not a crash");
        zalt_decoder_free(NULL); /* explicitly allowed */
        check(zalt_decoder_new(9, 0) == NULL, "out-of-range window log refused");

        uint8_t garbage[32] = {0};
        uint8_t out[16];
        size_t written = 0;
        int32_t rc = zalt_decompress(garbage, sizeof garbage, 23, out, sizeof out,
                                     &written);
        check(rc < 0, "garbage is refused");
        check(strcmp(zalt_error_message(rc), "not a modern zstd frame") == 0,
              "error message is specific");
    }

    printf("\n%s (%d failure%s)\n", failures ? "FAILED" : "passed", failures,
           failures == 1 ? "" : "s");
    return failures ? 1 : 0;
}
