//! Correctness on **valid** frames.
//!
//! # Why this target exists at all
//!
//! Random bytes essentially never form a valid zstd frame past the magic
//! number. A fuzzer fed raw input explores the rejection paths exhaustively and
//! the *decoding* paths almost not at all — it will hammer header validation
//! for days without ever building a Huffman table.
//!
//! So this target generates valid frames instead: the fuzzer's bytes are the
//! **content**, libzstd compresses them, and zalt must reproduce them exactly.
//! Every byte the fuzzer mutates changes what gets compressed, which changes
//! the entropy tables, the literal streams, and the sequences — so coverage
//! lands where the decoding actually happens.
//!
//! This is the shape that would have caught the FSE weight-stream
//! off-by-one recorded in `doc/ZALT-MAC.md` §10, which only showed up on the
//! highest-valued symbol in the Huffman table.

#![no_main]

use libfuzzer_sys::fuzz_target;

include!("shared.rs");

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let level = level_from(data[0]);
    let content = &data[1..];

    let Ok(frame) = zstd::stream::encode_all(content, level) else {
        return;
    };

    match zalt_codec::decompress(&frame, limits()) {
        Ok(decoded) => assert_eq!(
            decoded,
            content,
            "round trip failed at level {level} for {} bytes",
            content.len()
        ),
        Err(e) => panic!(
            "failed to decode a frame libzstd produced at level {level} \
             from {} bytes: {e}",
            content.len()
        ),
    }
});
