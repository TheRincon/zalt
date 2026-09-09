//! The `dcz` header parser, on inputs that are mostly *headers*.
//!
//! # Why this target is structured
//!
//! The naive version fed arbitrary bytes to `classify` and `parse_prelude`. It
//! survived 5.3M executions, which sounds like coverage and is not: the header
//! opens with a **64-bit** fixed sequence, so raw bytes reach the accepting
//! path with probability 2^-64. Essentially every execution went down a
//! rejection branch, and the branch that extracts a hash and splits a frame —
//! the one with arithmetic in it — was reached only via whatever the mutator
//! managed to preserve from a corpus entry.
//!
//! This is the same lesson `roundtrip_valid.rs` records for frames, applied to
//! a header: **build the shape, let the fuzzer choose the fields.** Here the
//! first byte picks a variant, so a meaningful fraction of executions carry a
//! real or near-real header — right magic with a wrong length, a header cut one
//! byte short, a `dcb` header, a valid header with an arbitrary hash — and the
//! remainder stays fully arbitrary so nothing is closed off.
//!
//! The properties checked are the ones a header parser can get wrong without
//! crashing:
//!
//! 1. neither entry point panics;
//! 2. `classify` and `parse_prelude` agree whenever sniffing concludes `dcz`;
//! 3. the frame handed back is exactly the input minus forty bytes, so no byte
//!    of the payload is lost or duplicated at the seam;
//! 4. a header built from known parts parses back to the parts it was built
//!    from — the round trip the arbitrary-bytes version could never reach.

#![no_main]

use libfuzzer_sys::fuzz_target;

use zalt_recipe::dcz::{self, DCB_MAGIC, PRELUDE_LEN, PRELUDE_MAGIC, Stream};

/// Build the input the parser actually sees, from a variant tag and a body.
///
/// Returns the bytes and, when the construction is a well-formed `dcz` header,
/// the hash and frame it was built from so the round trip can be asserted.
fn shape(tag: u8, body: &[u8]) -> (Vec<u8>, Option<([u8; 32], usize)>) {
    match tag % 8 {
        // A correct header with a fuzzer-chosen hash, then a fuzzer-chosen
        // frame. The case the naive target essentially never produced.
        0 | 1 | 2 => {
            let mut hash = [0u8; 32];
            let take = body.len().min(32);
            hash[..take].copy_from_slice(&body[..take]);
            let frame = if body.len() > 32 { &body[32..] } else { &[][..] };

            let mut out = PRELUDE_MAGIC.to_vec();
            out.extend_from_slice(&hash);
            out.extend_from_slice(frame);
            (out, Some((hash, frame.len())))
        }
        // Correct magic, header cut short. Every length below 40, so the
        // "need more bytes" boundary is walked rather than sampled.
        3 => {
            let mut out = PRELUDE_MAGIC.to_vec();
            out.extend_from_slice(body);
            let cut = if out.len() > PRELUDE_LEN {
                body.first().map_or(0, |b| usize::from(*b) % PRELUDE_LEN)
            } else {
                out.len()
            };
            out.truncate(cut);
            (out, None)
        }
        // The right skippable magic with the wrong length field: a legal zstd
        // skippable frame that is not a dcz header, which the two entry points
        // are required to disagree about.
        4 => {
            let mut out = PRELUDE_MAGIC.to_vec();
            let corrupt = body.first().copied().unwrap_or(0xff);
            out[4] = corrupt;
            out.extend_from_slice(body);
            (out, None)
        }
        // The sibling encoding, which must be refused by name.
        5 => {
            let mut out = DCB_MAGIC.to_vec();
            out.extend_from_slice(body);
            (out, None)
        }
        // A magic one byte off, exercising the near-miss the mutator would
        // otherwise have to invent.
        6 => {
            let mut out = PRELUDE_MAGIC.to_vec();
            let which = body.first().map_or(0, |b| usize::from(*b) % 8);
            out[which] ^= 1;
            out.extend_from_slice(body);
            (out, None)
        }
        // Wholly arbitrary, so nothing above closes off a shape nobody thought
        // to enumerate.
        _ => (body.to_vec(), None),
    }
}

fuzz_target!(|data: &[u8]| {
    let (tag, body) = data.split_first().map_or((0u8, &[][..]), |(t, r)| (*t, r));
    let (input, built) = shape(tag, body);

    let sniffed = dcz::classify(&input);
    let declared = dcz::parse_prelude(&input);

    match sniffed {
        Ok(Stream::Dictionary { hash, frame }) => {
            // The seam: everything after the header, and nothing else.
            assert_eq!(frame.len(), input.len() - PRELUDE_LEN);
            assert_eq!(frame, &input[PRELUDE_LEN..]);
            assert_eq!(hash.as_bytes(), &input[8..PRELUDE_LEN]);

            // Sniffing said dcz, so parsing must too, and identically.
            let (parsed_hash, parsed_frame) =
                declared.expect("classify found a dcz stream that parse_prelude rejected");
            assert_eq!(parsed_hash, hash);
            assert_eq!(parsed_frame, frame);

            // And when we built it, it must come back as what went in.
            if let Some((expected_hash, frame_len)) = built {
                assert_eq!(hash.as_bytes(), &expected_hash);
                assert_eq!(frame.len(), frame_len);
            }
        }
        Ok(Stream::Plain(rest)) => {
            assert_eq!(rest, input, "a plain stream is handed back whole");
            assert!(
                declared.is_err(),
                "parse_prelude accepted a stream classify called plain"
            );
            assert!(
                built.is_none(),
                "a header we built ourselves was classified as plain"
            );
        }
        Err(_) => {
            // Undecidable (short read) or refused (dcb). Reaching here at all
            // is the requirement; unwinding is the failure.
            assert!(
                built.is_none(),
                "a header we built ourselves failed to classify"
            );
        }
    }

    // The digest encodings, on whatever hash came out: fixed-size and
    // index-heavy, so worth running on values nobody chose.
    if let Ok(Stream::Dictionary { hash, .. }) = sniffed {
        let hex = hash.to_hex();
        assert!(hex.iter().all(|b| b.is_ascii_hexdigit()));
        let b64 = hash.to_base64();
        assert!(b64.iter().all(|b| b.is_ascii_graphic()));
    }
});
