//! Everything this encoder emits must be readable by the reference.
//!
//! # The direction that matters
//!
//! `roundtrip_valid` asks whether zalt can read what libzstd writes. This asks
//! the opposite, and it is the stronger question for an encoder: **libzstd is
//! the oracle for whether our output is a valid zstd frame at all.** Round
//! tripping through our own decoder proves only that the encoder and decoder
//! agree, and two halves of one codebase agreeing about a misreading is exactly
//! the failure an encoder invites.
//!
//! So the assertion is asymmetric on purpose. libzstd refusing a frame we wrote
//! is always our bug — there is no second opinion to appeal to, because the
//! reference *defines* what the ecosystem accepts. libzstd decoding it to the
//! wrong bytes is worse.
//!
//! The self round trip is checked too, but only after the reference has
//! agreed, so a failure there is unambiguous: the frame is valid and our
//! decoder is the one that is wrong.

#![no_main]

use libfuzzer_sys::fuzz_target;

use zalt_codec::encode::{BLOCK_SIZE_DEFAULT, EncodeParams, Strategy};

include!("shared.rs");

/// Parameters driven by the fuzzer, so the header shape varies as much as the
/// content does. The frame header's field widths depend on content size,
/// checksum, dictionary id and single-segment eligibility, and a fixed
/// parameter set would exercise exactly one path through all of it.
fn params_from(seed: [u8; 3], content_len: usize) -> EncodeParams {
    // 1 KiB up to the format maximum, in powers of two, so block splitting is
    // hit at every scale rather than only at 128 KiB.
    let block_log = 10 + u32::from((seed[0] & 0x7f) % 8);
    let block_size = (1u32 << block_log).min(BLOCK_SIZE_DEFAULT);

    // The window must admit the blocks, which `validate` enforces; picking it
    // from the block size keeps the fuzzer generating usable parameters
    // instead of spending its budget on refusals.
    let window_log = (block_log as u8).max(10);

    EncodeParams {
        // Every level, from bits of `seed[1]` the checksum does not use. A
        // level changes only which sequences are chosen — but greedy, lazy and
        // optimal parsing each choose them their own way, and the optimal
        // parser tracks repeat offsets per path. Fixed at 3, this target never
        // reached the code most likely to disagree with libzstd about them.
        level: 1 + i32::from((seed[1] >> 1) % 19),
        // Both strategies, so the sequence bitstream is exercised on whatever
        // match structure the mutator invents. This is the part of the encoder
        // with arithmetic in it, and the part libzstd is the only real oracle
        // for: an off-by-one in the state flush produces a stream that decodes
        // to *something*, and only the reference says whether it is the right
        // something.
        strategy: if seed[0] & 0x80 == 0 {
            Strategy::Stored
        } else {
            Strategy::Compress
        },
        window_log,
        checksum: seed[1] & 1 == 1,
        dictionary_id: match seed[2] % 4 {
            0 => None,
            1 => Some(1),
            2 => Some(0x1234),
            _ => Some(0x5A54_4D4C),
        },
        block_size,
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 3 {
        return;
    }
    let params = params_from([data[0], data[1], data[2]], data.len() - 3);
    let content = &data[3..];

    let frame = match zalt_codec::compress(content, &params) {
        Ok(f) => f,
        // `validate` refusing a parameter set is a legitimate outcome and not
        // interesting; the encoder must never produce a frame it knows is
        // malformed.
        Err(_) => return,
    };

    // A declared dictionary id demands a dictionary neither decoder holds, so
    // only the header shape is under test there — that the frame parses at all
    // up to the point the dictionary is required.
    if params.dictionary_id.is_some() {
        let err = zalt_codec::decompress(&frame, limits());
        assert!(
            matches!(err, Err(zalt_codec::Error::DictionaryRequired { .. })),
            "a frame declaring a dictionary should ask for it, got {err:?}"
        );
        return;
    }

    // The reference is the oracle. Its refusal is our bug.
    match zstd::stream::decode_all(&frame[..]) {
        Ok(decoded) => assert_eq!(
            decoded,
            content,
            "libzstd decoded our frame to different bytes ({} in, block_size {}, checksum {})",
            content.len(),
            params.block_size,
            params.checksum
        ),
        Err(e) => panic!(
            "libzstd refused a frame we wrote: {e} ({} bytes in, block_size {}, \
             window_log {}, checksum {})",
            content.len(),
            params.block_size,
            params.window_log,
            params.checksum
        ),
    }

    // Only now is a self round-trip failure unambiguous.
    match zalt_codec::decompress(&frame, limits()) {
        Ok(decoded) => assert_eq!(decoded, content, "our decoder disagrees with libzstd"),
        Err(zalt_codec::Error::WindowTooLarge { .. }) => {}
        Err(e) => panic!("our decoder refused a frame libzstd accepted: {e}"),
    }
});
