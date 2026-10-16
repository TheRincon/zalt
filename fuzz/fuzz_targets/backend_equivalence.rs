//! Scalar and NEON backends must be byte-identical on every input.
//!
//! This is what makes the L1/L1a seam trustworthy. `zalt-neon` is the only
//! crate permitted to contain `unsafe`, and its kernels assume a contract they
//! cannot check in release builds — so the scalar path is the oracle, and this
//! target is what holds the two together on inputs nobody wrote by hand.
//!
//! Both a valid frame and the raw bytes are tried: the former reaches match
//! execution with realistic offset and length distributions, the latter reaches
//! it with whatever a mutator invents.

#![no_main]

use libfuzzer_sys::fuzz_target;
use zalt_codec::Scalar;
use zalt_codec::decompress_with;

include!("shared.rs");

#[cfg(target_arch = "aarch64")]
type Accel = zalt_neon::Neon;
#[cfg(not(target_arch = "aarch64"))]
type Accel = Scalar;

fn compare(frame: &[u8]) {
    let scalar = decompress_with::<Scalar>(frame, limits());
    let accel = decompress_with::<Accel>(frame, limits());

    match (scalar, accel) {
        (Ok(a), Ok(b)) => assert_eq!(a, b, "backends disagree on decoded output"),
        (Err(a), Err(b)) => assert_eq!(
            a, b,
            "backends disagree on which error a malformed frame produces"
        ),
        (a, b) => panic!("backends disagree on validity: scalar={a:?} accel={b:?}"),
    }
}

/// The encoder's kernel: `common_prefix` on the input against itself shifted
/// — which is how a match finder uses it, and where repetitive input makes the
/// agreement long — and on two halves of different lengths.
fn compare_prefixes(data: &[u8]) {
    use zalt_codec::Backend;
    for shift in [1usize, 2, 3, 7, 8, 9, 15, 16, 17, 31, 64] {
        if let Some(later) = data.get(shift..) {
            assert_eq!(
                Scalar::common_prefix(data, later),
                Accel::common_prefix(data, later),
                "common_prefix disagrees at shift {shift}"
            );
        }
    }
    let (a, b) = data.split_at(data.len() / 3);
    assert_eq!(Scalar::common_prefix(a, b), Accel::common_prefix(a, b));
}

/// Whole frames: a backend may change how fast the encoder measures, never
/// what it encodes. Over the first 16 KiB only: the corpus holds inputs up to
/// a megabyte, and encoding those at level 19 took this target from decoding
/// speed to 54 runs a second. The kernel itself still sees every byte, in
/// `compare_prefixes`.
fn compare_encodes(level: u8, content: &[u8]) {
    use zalt_codec::encode::{EncodeParams, Strategy, compress_with};
    let content = &content[..content.len().min(16 << 10)];
    let params = EncodeParams {
        strategy: Strategy::Compress,
        level: i32::from(level % 19) + 1,
        ..Default::default()
    };
    let scalar = compress_with::<Scalar>(content, &params);
    let accel = compress_with::<Accel>(content, &params);
    assert_eq!(scalar, accel, "backends encode differently at {params:?}");
}

fuzz_target!(|data: &[u8]| {
    compare(data);
    compare_prefixes(data);

    if data.len() >= 2 {
        compare_encodes(data[0], &data[1..]);
        if let Ok(frame) = zstd::stream::encode_all(&data[1..], level_from(data[0])) {
            compare(&frame);
        }
    }
});
