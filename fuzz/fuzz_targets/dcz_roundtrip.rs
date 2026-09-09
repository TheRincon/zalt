//! A complete `dcz` stream, built by the reference and taken apart by us.
//!
//! # What this reaches that the header parser does not
//!
//! `dcz_prelude` checks forty bytes. This checks the mechanism those forty
//! bytes exist to drive: hash a dictionary, find it again by that hash, and
//! decode a frame that is *meaningless without it*.
//!
//! The fuzzer's bytes become the dictionary and the payload, libzstd compresses
//! one against the other, and we rebuild the stream an origin would send. So
//! every mutation changes the dictionary content, which changes which matches
//! the encoder finds, which changes the offsets that reach back past the start
//! of the frame — the dictionary-specific decode path, which no amount of
//! arbitrary input would ever construct.
//!
//! Four properties, each a real failure this could have:
//!
//! 1. **round trip.** Decoding the resolved frame reproduces the payload
//!    exactly. A dictionary seeded at the wrong offset, or reachable for one
//!    byte too long, produces plausible output rather than an error — this is
//!    the only check that catches it.
//! 2. **identity.** The store files the dictionary under the same hash the
//!    stream names it by, so `resolve` finds it.
//! 3. **the seam.** The frame handed back by `resolve` is byte-identical to
//!    what libzstd produced, so the split is not off by one.
//! 4. **transparency.** Decoding the *whole* stream, header included, gives the
//!    same answer as decoding the frame alone — the skippable-frame property
//!    the format depends on, checked on every input rather than once.

#![no_main]

use libfuzzer_sys::fuzz_target;

use zalt_recipe::dcz::{DictionaryStore, PRELUDE_MAGIC};
use zalt_recipe::sha256::Sha256;

include!("shared.rs");

/// Keep both halves small enough that an execution is fast.
///
/// A fuzzer that spends a millisecond per run explores a thousand times less
/// than one that spends a microsecond, and nothing here needs a large
/// dictionary to exercise a dictionary-relative offset.
const MAX_DICT: usize = 16 * 1024;
const MAX_PAYLOAD: usize = 16 * 1024;

fuzz_target!(|data: &[u8]| {
    // 1 byte level, 2 bytes split point, then the material.
    if data.len() < 4 {
        return;
    }
    let level = level_from(data[0]);
    let split_seed = usize::from(u16::from_le_bytes([data[1], data[2]]));
    let material = &data[3..];

    // A dictionary of zero bytes is not a dictionary; libzstd ignores it and
    // the run would silently test the no-dictionary path instead.
    let split = 1 + (split_seed % material.len().max(1));
    let split = split.min(material.len());
    if split == 0 || split == material.len() {
        return;
    }
    let dictionary = &material[..split.min(MAX_DICT)];
    let payload = &material[split..][..(material.len() - split).min(MAX_PAYLOAD)];
    if dictionary.is_empty() || payload.is_empty() {
        return;
    }

    // The reference compresses against the dictionary. `with_dictionary` feeds
    // it to ZSTD_CCtx_loadDictionary, which reads a blob with no dictionary
    // magic as raw content — exactly what RFC 9842 §5 specifies for `dcz`.
    let Ok(mut compressor) = zstd::bulk::Compressor::with_dictionary(level, dictionary) else {
        return;
    };
    let Ok(frame) = compressor.compress(payload) else {
        return;
    };

    // The stream an origin would send.
    let hash = Sha256::digest(dictionary);
    let mut stream = PRELUDE_MAGIC.to_vec();
    stream.extend_from_slice(hash.as_bytes());
    stream.extend_from_slice(&frame);

    // --- 2. identity: filed under the hash the wire names it by.
    let mut store = DictionaryStore::new();
    let Ok(filed) = store.insert(dictionary) else {
        return;
    };
    assert_eq!(filed, hash, "the store disagreed with the wire about identity");

    let resolved = store
        .resolve(&stream)
        .expect("a stream we built must be well formed")
        .expect("the dictionary we just inserted must be found");
    let (dictionary_handle, split_frame) = resolved;

    // --- 3. the seam.
    assert_eq!(
        split_frame, &frame[..],
        "resolve split the stream at the wrong offset"
    );

    // --- 1. round trip.
    let mut decoder = zalt_codec::Decoder::<zalt_codec::Scalar>::new(limits());
    decoder.load_dictionary(dictionary_handle.clone());
    let mut out = Vec::new();
    match decoder.decompress(split_frame, &mut out) {
        Ok(_) => assert_eq!(
            out,
            payload,
            "round trip failed at level {level} with a {}-byte dictionary and \
             a {}-byte payload",
            dictionary.len(),
            payload.len()
        ),
        Err(e) => {
            // A window cap refusal is the harness being stricter than libzstd,
            // not a bug: `limits()` caps the window below what a high level may
            // choose. Anything else is a frame libzstd produced that we cannot
            // read, which is the bug this target exists to find.
            if !matches!(e, zalt_codec::Error::WindowTooLarge { .. }) {
                panic!(
                    "failed to decode a dictionary-compressed frame libzstd \
                     produced at level {level}: {e}"
                );
            }
            return;
        }
    }

    // --- 4. transparency: the header is a skippable frame, so decoding the
    // whole stream must give the same answer as decoding the frame alone.
    let mut whole = zalt_codec::Decoder::<zalt_codec::Scalar>::new(limits());
    whole.load_dictionary(dictionary_handle.clone());
    let mut through_header = Vec::new();
    whole
        .decompress(&stream, &mut through_header)
        .expect("the dcz header must be skipped like any skippable frame");
    assert_eq!(
        through_header, out,
        "decoding through the header differed from decoding the frame alone"
    );
});
