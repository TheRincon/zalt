//! The Central Index's record decoder, past its checksum.
//!
//! Random bytes would never get here: every index carries an XXH64 over itself
//! and a compressed body, so a fuzzer flipping bits is stopped at the checksum
//! on every run. So the fuzzer's bytes are taken as the *decompressed body* —
//! the entry records themselves — and wrapped exactly as a writer would wrap
//! them: compressed, framed, counted and signed. What is fuzzed is then the
//! part a hostile writer controls: every field of every record, and the count.
//!
//! Anything accepted must also survive a re-encode and parse back to itself —
//! so the parser cannot accept a record it reads differently from how the
//! writer would write it.

#![no_main]

use libfuzzer_sys::fuzz_target;
use zalt_archive::index::{Index, IndexLimits};

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let count = u64::from(data[0]);
    let hash_algorithm = data[1] % 4;
    let body = &data[2..];

    let Ok(frame) = zalt_codec::encode::compress(
        body,
        &zalt_codec::encode::EncodeParams {
            strategy: zalt_codec::encode::Strategy::Compress,
            level: 1,
            checksum: true,
            ..Default::default()
        },
    ) else {
        return;
    };
    let mut user_data = Vec::with_capacity(24 + frame.len() + 8);
    user_data.extend_from_slice(b"ZSTI");
    user_data.push(1);
    user_data.push(hash_algorithm);
    user_data.extend_from_slice(&0u16.to_le_bytes());
    user_data.extend_from_slice(&count.to_le_bytes());
    user_data.extend_from_slice(&u64::MAX.to_le_bytes()); // no stream bound
    user_data.extend_from_slice(&frame);
    let sum = zalt_codec::xxh64::xxh64(&user_data, 0);
    user_data.extend_from_slice(&sum.to_le_bytes());

    let limits = IndexLimits {
        max_entries: 1024,
        max_body: 1 << 20,
        max_path: 4096,
        max_xattrs_per_entry: 256,
    };
    if let Ok(index) = Index::parse(&user_data, &limits) {
        let again = index.encode(1).expect("an accepted index re-encodes");
        let back = Index::parse(&again, &limits).expect("and parses back");
        assert_eq!(back, index, "an accepted index must mean one thing");
    }
});
