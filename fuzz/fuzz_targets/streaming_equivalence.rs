//! The same frame, split at fuzzer-chosen boundaries, must decode identically
//! to the one-shot result — and the caller must be free to discard output as it
//! goes.
//!
//! # Why a dedicated target
//!
//! This is the bug class unit tests structurally cannot reach. Every individual
//! state transition can be correct while the machine still loses or duplicates
//! a byte when suspended at one specific offset. The fixed-size chunking in the
//! integration tests covers a handful of schedules; a fuzzer explores the ones
//! nobody thought to write down — a chunk that ends exactly between a block
//! header's second and third byte, or mid-way through a four-stream jump table.
//!
//! The output buffer is **cleared between calls** on purpose. That exercises the
//! window's independence from the caller's buffer, so a regression that
//! reintroduced the old "history lives in the caller's Vec" assumption would
//! fail here rather than silently working in tests that never drain.

#![no_main]

use libfuzzer_sys::fuzz_target;
use zalt_codec::decode::{Decoder, Status};
use zalt_codec::Scalar;

include!("shared.rs");

fuzz_target!(|data: &[u8]| {
    if data.len() < 3 {
        return;
    }
    let level = level_from(data[0]);
    let schedule_seed = &data[..2];
    let content = &data[2..];

    let Ok(frame) = zstd::stream::encode_all(content, level) else {
        return;
    };

    // One-shot reference.
    let Ok(expected) = zalt_codec::decompress(&frame, limits()) else {
        return;
    };

    let mut decoder = Decoder::<Scalar>::new(limits());
    let mut scratch = Vec::new();
    let mut collected = Vec::new();
    let mut cursor = 0usize;
    let mut last = Status::NeedMoreInput;

    let sizes = chunk_schedule(schedule_seed);
    let mut index = 0usize;
    let mut zero_run = 0usize;

    while cursor < frame.len() {
        let mut size = sizes[index % sizes.len()];
        index += 1;

        // Feed empty slices sometimes, but never forever: a schedule of all
        // zeros would otherwise loop without consuming anything.
        if size == 0 {
            zero_run += 1;
            if zero_run > 2 {
                size = 1;
                zero_run = 0;
            }
        } else {
            zero_run = 0;
        }

        let end = (cursor + size).min(frame.len());
        let progress = match decoder.decompress(&frame[cursor..end], &mut scratch) {
            Ok(p) => p,
            Err(e) => panic!("chunked decode failed where one-shot succeeded: {e}"),
        };

        if end == cursor {
            assert_eq!(progress.consumed, 0, "consumed input from an empty slice");
            assert_eq!(
                progress.status,
                Status::NeedMoreInput,
                "an empty slice must ask for more input"
            );
            continue;
        }
        assert!(
            progress.consumed > 0,
            "no forward progress on a non-empty slice"
        );

        cursor += progress.consumed;
        last = progress.status;

        // Drain and discard: history must not depend on this buffer.
        collected.extend_from_slice(&scratch);
        scratch.clear();
    }

    assert_eq!(last, Status::FrameComplete, "frame did not complete");
    assert_eq!(collected, expected, "chunked output differs from one-shot");
});
