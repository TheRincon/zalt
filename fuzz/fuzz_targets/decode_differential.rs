//! Differential decode against libzstd on arbitrary input.
//!
//! # The oracle must be asymmetric — and weaker than it first appears
//!
//! It is tempting to assert "zalt and libzstd agree". They must not, in either
//! direction, and working out why is most of the value of this target.
//!
//! **zalt supports less.** It rejects legacy v0.1–v0.7 frames by design, does
//! not yet load dictionaries, and enforces a window cap libzstd does not.
//!
//! **Corrupt frames have no defined behaviour.** A zstd frame without a content
//! checksum carries no way to detect a flipped bit. Feed one to two conformant
//! decoders and they may produce different garbage, or one may reject where the
//! other does not — and neither is wrong. A mutating fuzzer produces exactly
//! this input constantly, so demanding agreement here reports noise forever.
//! (The first run of this target did: a one-byte flip inside a Huffman literal
//! stream, which libzstd decoded to garbage and zalt refused.)
//!
//! What survives is genuinely worth checking:
//!
//! | zalt | libzstd | verdict |
//! |---|---|---|
//! | accepts | rejects | **always a bug** — a hole in validation |
//! | accepts | accepts, frame **has a checksum** | outputs must be identical |
//! | accepts | accepts, no checksum | no assertion possible |
//! | rejects | accepts | fine — zalt may be stricter |
//!
//! Row one is the security-relevant one: accepting a frame libzstd refuses
//! means the validation set has a gap, and gaps in a decoder's validation are
//! how memory-safety bugs become reachable.
//!
//! Row two is strengthened by the checksum: if a frame declares one and *both*
//! decoders accept, both verified the same XXH64 over their output, so the
//! outputs must agree. That turns an unusable assertion into a real one for
//! every frame `zstd --check` produces — which is the CLI default.
//!
//! The unconditional "output must match" check lives in `roundtrip_valid`,
//! where frames are valid by construction and no such reasoning is needed.

#![no_main]

use libfuzzer_sys::fuzz_target;

include!("shared.rs");

/// Whether a frame's header declares a content checksum.
///
/// Only meaningful for a single, well-formed frame header; anything else
/// returns false and the caller simply makes no assertion.
fn declares_checksum(data: &[u8]) -> bool {
    if data.len() < 5 {
        return false;
    }
    let magic = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    if magic != 0xFD2F_B528 {
        return false;
    }
    data[4] & 0b0000_0100 != 0
}

fuzz_target!(|data: &[u8]| {
    let ours = zalt_codec::decompress(data, limits());
    let theirs = zstd::stream::decode_all(data).ok();

    match (&ours, &theirs) {
        (Ok(a), None) => panic!(
            "zalt accepted {} bytes that libzstd rejects, producing {} bytes",
            data.len(),
            a.len()
        ),
        (Ok(a), Some(b)) => {
            if declares_checksum(data) {
                assert_eq!(
                    a, b,
                    "outputs differ on a checksummed frame both decoders accepted"
                );
            }
        }
        // zalt may legitimately be stricter, and on an unchecksummed corrupt
        // frame neither decoder owes the other anything.
        (Err(_), _) => {}
    }
});
