//! The L1 / L1a acceleration seam.
//!
//! L1 defines the small set of operations worth accelerating and ships a scalar
//! implementation of each. `zalt-neon` provides an alternative. Selection is
//! **compile-time**, via a generic parameter — there is no runtime dispatch,
//! because on aarch64 NEON is architecturally mandatory and there is nothing to
//! detect. (This is a genuine simplification over the x86 SSE/AVX2/AVX-512
//! dispatch problem; see `doc/ZALT-MAC.md` §5.)
//!
//! ## Why the scalar path is never deleted
//!
//! [`Scalar`] is not a fallback for platforms we do not care about. It is:
//!
//! 1. **The correctness oracle.** `backend_equivalence` fuzzing compares every
//!    accelerated kernel against it on identical input.
//! 2. **The Miri target.** Miri cannot execute NEON intrinsics, so the scalar
//!    path is the only way to get Miri's UB detection over this code at all.
//! 3. **The portability story**, which keeps `zalt-codec` a general zstd crate
//!    rather than an Apple-only one.
//!
//! Deleting it would forfeit all three.
//!
//! ## Contract
//!
//! Implementations may assume arguments are already validated and are permitted
//! to use unchecked accesses on that basis. Callers must go through [`crate::lz`],
//! which validates first. Implementing this trait is therefore a safety-critical
//! act even though the trait itself is safe: an implementation that trusts the
//! contract will read out of bounds if the contract is broken.

/// Operations a backend may accelerate.
///
/// All methods have byte-exact semantics defined by [`Scalar`]. An
/// implementation that differs from `Scalar` on any valid input is wrong, and
/// the equivalence tests exist to catch exactly that.
pub trait Backend: Copy + Clone + Send + Sync + 'static {
    /// Human-readable name, for benchmark output and error reporting.
    const NAME: &'static str;

    /// Copy `src` into `dst`, non-overlapping.
    ///
    /// # Contract
    ///
    /// `dst.len() == src.len()`.
    fn copy_literals(dst: &mut [u8], src: &[u8]);

    /// LZ77 match copy within a single buffer.
    ///
    /// Copies `len` bytes to `buf[pos..]`, reading from `buf[pos - offset..]`,
    /// with **byte-at-a-time semantics**: when `offset < len` the source region
    /// overlaps the destination and the result is the `offset`-periodic
    /// repetition of the preceding bytes. This is not `memmove`, and using
    /// `memmove` here is a classic way to produce a decoder that is subtly
    /// wrong on exactly the inputs real data produces most often.
    ///
    /// # Contract
    ///
    /// - `offset >= 1`
    /// - `offset <= pos`
    /// - `pos + len <= buf.len()`
    fn copy_match(buf: &mut [u8], pos: usize, offset: usize, len: usize);
}

/// Portable scalar backend. The reference semantics for every other backend.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Scalar;

impl Backend for Scalar {
    const NAME: &'static str = "scalar";

    #[inline]
    fn copy_literals(dst: &mut [u8], src: &[u8]) {
        debug_assert_eq!(dst.len(), src.len());
        dst.copy_from_slice(src);
    }

    #[inline]
    fn copy_match(buf: &mut [u8], pos: usize, offset: usize, len: usize) {
        debug_assert!(offset >= 1);
        debug_assert!(offset <= pos);
        debug_assert!(pos + len <= buf.len());

        // Deliberately the most obvious formulation that can be written: a
        // direct transcription of "byte i of the match is the byte `offset`
        // back from it". This is the oracle. Clarity is the whole requirement;
        // speed is what the other backends are for.
        //
        // Note this reads `buf` as it is being written, which is exactly the
        // semantics — for `offset < len` the source region overlaps and the
        // result is the `offset`-periodic extension. An iterator-based
        // rewrite cannot express that, because it would need simultaneous
        // shared and mutable borrows of `buf`.
        for i in 0..len {
            buf[pos + i] = buf[pos - offset + i];
        }
    }
}
