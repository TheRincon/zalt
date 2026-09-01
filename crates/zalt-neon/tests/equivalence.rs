//! `backend_equivalence` — the test that makes the L1/L1a seam trustworthy.
//!
//! Every accelerated kernel must be byte-exact with `Scalar` on every input
//! satisfying the `Backend` contract. This is the M0 form: exhaustive over the
//! interesting parameter space. `zalt-fuzz` grows a continuous version of the
//! same property in M3.
//!
//! The offsets below are not arbitrary. `1..=16` covers the whole overlapping
//! regime plus the boundary where the kernel switches strategy; the values
//! dividing 16 (1, 2, 4, 8, 16) take the degenerate `step == 0` path where the
//! rotation is the identity, which is exactly where an off-by-one in the
//! pattern rotation would hide.

#![cfg(target_arch = "aarch64")]

use zalt_codec::{Backend, Scalar};
use zalt_neon::Neon;

/// Deterministic filler. A real PRNG would be a dependency for no benefit; what
/// matters is that bytes differ so a misplaced copy is visible.
fn fill(buf: &mut [u8], seed: u8) {
    let mut x = seed | 1;
    for b in buf.iter_mut() {
        x = x.wrapping_mul(37).wrapping_add(11);
        *b = x;
    }
}

fn check(prefix_len: usize, offset: usize, len: usize, seed: u8) {
    let total = prefix_len + len + 64; // slack, so both kernels see the same shape
    let mut a = vec![0u8; total];
    fill(&mut a[..prefix_len], seed);
    let mut b = a.clone();

    Scalar::copy_match(&mut a, prefix_len, offset, len);
    Neon::copy_match(&mut b, prefix_len, offset, len);

    assert_eq!(
        a, b,
        "mismatch: prefix={prefix_len} offset={offset} len={len} seed={seed}"
    );
}

#[test]
fn exhaustive_small_offsets_and_lengths() {
    for offset in 1..=16usize {
        for len in 0..=80usize {
            check(offset.max(1), offset, len, 0xA5);
        }
    }
}

#[test]
fn offsets_spanning_the_strategy_boundary() {
    // 15/16/17 is where copy_match_narrow hands over to copy_match_wide.
    for offset in [14, 15, 16, 17, 18, 31, 32, 33, 64, 127, 128] {
        for len in [0, 1, 15, 16, 17, 31, 32, 33, 64, 100, 255] {
            check(offset, offset, len, 0x3C);
        }
    }
}

#[test]
fn long_runs_from_tiny_offsets() {
    // The pathological shape: one byte smeared across kilobytes. Also the shape
    // that indentation and padding actually produce.
    for offset in 1..=8usize {
        for len in [256, 1024, 4096] {
            check(offset, offset, len, 0x11);
        }
    }
}

#[test]
fn match_source_deep_in_a_large_buffer() {
    // Guards against any assumption that the match sits near position 0.
    for offset in [1, 3, 7, 16, 100, 4096] {
        for len in [1, 33, 500] {
            check(8192, offset, len, 0x7E);
        }
    }
}

#[test]
fn narrow_kernel_falls_back_when_the_load_would_overrun() {
    // No trailing slack: the 16-byte pattern load from `pos - offset` would read
    // past the end, so the kernel must take the scalar path and still be exact.
    for offset in 1..=15usize {
        for len in 1..=8usize {
            let prefix = offset;
            let total = prefix + len; // exactly sized, zero slack
            let mut a = vec![0u8; total];
            fill(&mut a[..prefix], 0x5A);
            let mut b = a.clone();

            Scalar::copy_match(&mut a, prefix, offset, len);
            Neon::copy_match(&mut b, prefix, offset, len);

            assert_eq!(a, b, "no-slack mismatch: offset={offset} len={len}");
        }
    }
}

#[test]
fn literals_copy_matches() {
    for len in [0usize, 1, 15, 16, 17, 64, 1000] {
        let mut src = vec![0u8; len];
        fill(&mut src, 0x9D);
        let mut a = vec![0u8; len];
        let mut b = vec![0u8; len];
        Scalar::copy_literals(&mut a, &src);
        Neon::copy_literals(&mut b, &src);
        assert_eq!(a, b);
        assert_eq!(a, src);
    }
}

/// The property stated directly, independent of `Scalar`: a match copy is the
/// `offset`-periodic extension of the bytes preceding `pos`. If both backends
/// were wrong in the same way, the tests above would agree and this would not.
#[test]
fn periodic_extension_property() {
    for offset in 1..=20usize {
        let len = 137;
        let total = offset + len + 64;
        let mut buf = vec![0u8; total];
        fill(&mut buf[..offset], 0x42);
        let seed_region: Vec<u8> = buf[..offset].to_vec();

        Neon::copy_match(&mut buf, offset, offset, len);

        for i in 0..len {
            assert_eq!(
                buf[offset + i],
                seed_region[i % offset],
                "offset={offset} i={i}"
            );
        }
    }
}

/// End-to-end backend equivalence on **real zstd output**.
///
/// The kernel tests above drive `copy_match` with synthetic shapes. This drives
/// it the way a decoder actually does: through a compressed block's sequence
/// execution, with whatever offsets and lengths `zstd -3` chose. A kernel can
/// pass every synthetic case and still be wrong on the distribution real data
/// produces.
#[test]
fn decoding_real_zstd_agrees_between_backends() {
    use zalt_codec::decompress_with;
    use zalt_codec::params::DecodeLimits;

    const TEXT_ZST: &[u8] = include_bytes!("../../zalt-codec/tests/fixtures/text.zst");
    const TEXT_BIN: &[u8] = include_bytes!("../../zalt-codec/tests/fixtures/text.bin");

    let scalar = decompress_with::<Scalar>(TEXT_ZST, DecodeLimits::default()).unwrap();
    let neon = decompress_with::<Neon>(TEXT_ZST, DecodeLimits::default()).unwrap();

    assert_eq!(scalar, TEXT_BIN, "scalar backend must decode correctly");
    assert_eq!(neon, TEXT_BIN, "neon backend must decode correctly");
    assert_eq!(scalar, neon, "backends must agree byte for byte");
}
