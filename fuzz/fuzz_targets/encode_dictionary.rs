//! Everything the encoder writes against a dictionary, libzstd must read with
//! that dictionary — and the scalar and NEON backends must write it alike.
//!
//! The dictionary adds one rule an encoder can get wrong in a way only the
//! reference will catch: RFC 8878 §5 lets a match reach into the dictionary,
//! further back than `Window_Size` even, only while the frame has produced at
//! most `Window_Size` bytes. So the window is often small here — 1 KiB up —
//! and the dictionary often *is* part of the content, so the encoder keeps
//! wanting to reach into it after it may no longer.
//!
//! The dictionary is raw content: the fuzzer's bytes have no ID or tables to
//! get right, and a raw dictionary is exactly the history half of a
//! formatted one, which is the half the encoder uses. libzstd loads it the
//! same way (`ZSTD_DCtx_loadDictionary` on bytes without the magic).

#![no_main]

use std::io::Read;

use libfuzzer_sys::fuzz_target;

use zalt_codec::dict::Dictionary;
use zalt_codec::encode::{EncodeParams, Strategy, compress_with_dictionary};

include!("shared.rs");

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    let (seed, rest) = data.split_at(4);
    // The dictionary: a slice of the content itself half the time, so matches
    // into it are plentiful; otherwise bytes of its own.
    let dict_len = (usize::from(seed[2]) * 64 + 8).min(rest.len());
    let (dict_bytes, content) = if seed[3] & 1 == 0 {
        rest.split_at(dict_len)
    } else {
        (&rest[..dict_len], rest)
    };
    if dict_bytes.len() < 8 || dict_bytes[..4] == 0xEC30_A437u32.to_le_bytes() {
        return;
    }
    let Ok(dictionary) = Dictionary::parse(dict_bytes) else {
        return;
    };

    // Windows from 1 KiB to 128 KiB, the block no larger than the window.
    let window_log = 10 + (seed[1] & 0x07);
    let params = EncodeParams {
        level: 1 + i32::from(seed[0] % 19),
        strategy: Strategy::Compress,
        window_log,
        checksum: seed[1] & 0x80 != 0,
        dictionary_id: None,
        block_size: 1 << window_log.min(17),
    };

    let scalar = match compress_with_dictionary::<zalt_codec::Scalar>(content, &dictionary, &params) {
        Ok(f) => f,
        Err(_) => return,
    };
    let neon = compress_with_dictionary::<zalt_neon::Selected>(content, &dictionary, &params)
        .expect("the scalar backend encoded it");
    assert!(scalar == neon, "the backends wrote different frames");

    // The reference, given the same dictionary, is the oracle.
    let mut decoded = Vec::new();
    match zstd::stream::read::Decoder::with_dictionary(&scalar[..], dict_bytes)
        .and_then(|mut d| d.read_to_end(&mut decoded))
    {
        Ok(_) => assert_eq!(decoded, content, "libzstd decoded our frame to different bytes"),
        Err(e) => panic!(
            "libzstd refused a frame we wrote: {e} ({} bytes, dictionary {}, window_log {window_log}, level {})",
            content.len(),
            dict_bytes.len(),
            params.level
        ),
    }

    // Only now is a self round-trip failure unambiguous.
    let mut ours = zalt_codec::Decoder::<zalt_codec::Scalar>::new(limits());
    ours.load_dictionary(dictionary);
    let mut out = Vec::new();
    ours.decompress(&scalar, &mut out)
        .unwrap_or_else(|e| panic!("our decoder refused a frame libzstd accepted: {e}"));
    assert_eq!(out, content, "our decoder disagrees with libzstd");
});
