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

fuzz_target!(|data: &[u8]| {
    compare(data);

    if data.len() >= 2 {
        if let Ok(frame) = zstd::stream::encode_all(&data[1..], level_from(data[0])) {
            compare(&frame);
        }
    }
});
