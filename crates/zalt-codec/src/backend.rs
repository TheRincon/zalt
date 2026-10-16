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

    /// How many leading bytes `a` and `b` share — the encoder's match length.
    ///
    /// The one encoder kernel. Every match finder measures every candidate
    /// with it, and at the deeper levels it is most of the encode: 71% of
    /// level 13's time over a deferred tree, where comparisons run long.
    ///
    /// **No contract**, unlike the decoding kernels: any two slices, of any
    /// lengths, and the answer is at most the shorter. An implementation must
    /// not read past either. A wrong answer here is a wrong match length, and
    /// a frame that decodes to the wrong bytes, so the equivalence tests
    /// compare whole encoded frames, not just this function.
    fn common_prefix(a: &[u8], b: &[u8]) -> usize;
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

    /// **A word at a time.** Not the most obvious form, as `copy_match` is,
    /// because this one is also what every build without NEON encodes with —
    /// the match finder's own loop from before this seam existed, moved here
    /// unchanged. A byte-at-a-time `take_while` walks it with a compare and
    /// an unpredictable branch per byte; comparing eight bytes at once turns
    /// that into an XOR and a `trailing_zeros`, which on Firestorm is
    /// `eor`/`rbit`/`clz`: three instructions for what was eight iterations.
    ///
    /// `trailing_zeros` finds the lowest *bit* that differs, and because the
    /// load is little-endian the lowest differing bit lies in the lowest
    /// differing byte — so `>> 3` is the byte index, with no endianness
    /// correction. A big-endian host would need `leading_zeros` instead, which
    /// is why the load is explicitly `from_le_bytes` rather than native.
    ///
    /// No bounds check in the loop: `first_chunk` returns `None` at the end of
    /// the shorter slice, which is also the loop's exit condition. `matched`
    /// only advances after *both* chunks are produced, so the slicing below it
    /// can never be out of range.
    #[inline]
    fn common_prefix(a: &[u8], b: &[u8]) -> usize {
        const WORD: usize = 8;

        let mut matched = 0usize;
        while let (Some(x), Some(y)) = (
            a[matched..].first_chunk::<WORD>(),
            b[matched..].first_chunk::<WORD>(),
        ) {
            let diff = u64::from_le_bytes(*x) ^ u64::from_le_bytes(*y);
            if diff != 0 {
                return matched + (diff.trailing_zeros() >> 3) as usize;
            }
            matched += WORD;
        }

        // Fewer than eight bytes left on one side. At most seven iterations.
        matched
            + a[matched..]
                .iter()
                .zip(&b[matched..])
                .take_while(|(x, y)| x == y)
                .count()
    }
}
