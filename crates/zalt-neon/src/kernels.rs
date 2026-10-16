//! NEON kernels.
//!
//! Byte-exact with [`zalt_codec::Scalar`] on every input satisfying the
//! [`Backend`] contract. `tests/equivalence.rs` is what holds that claim up.

use core::arch::aarch64::{
    uint8x16_t, vceqq_u8, vget_lane_u64, vld1q_u8, vmvnq_u8, vqtbl1q_u8, vreinterpret_u64_u8,
    vreinterpretq_u16_u8, vshrn_n_u16, vst1q_u8,
};
use zalt_codec::Backend;

/// NEON backend for aarch64.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Neon;

/// Bytes moved per vector operation.
const VEC: usize = 16;

impl Backend for Neon {
    const NAME: &'static str = "neon";

    // `always`, not a hint: these are reached once per sequence from the
    // executor, and `#[inline]` was declined there exactly as it was on the bit
    // reader. A call here re-spills the window cursor the caller just computed.
    #[inline(always)]
    fn copy_literals(dst: &mut [u8], src: &[u8]) {
        debug_assert_eq!(dst.len(), src.len());
        // Bulk copy is still Apple's `memcpy`: it is NEON-optimised, tuned per
        // core generation and aware of the 128-byte cache line, and a
        // hand-rolled `vld1q`/`vst1q` loop measured against it is a regression
        // dressed up as an optimisation. That was the M0 finding and it stands.
        //
        // What did *not* stand is applying it at every length. A literal run
        // between two matches is a handful of bytes, and `memcpy` must branch
        // on a length it cannot see before moving any of them — the call costs
        // more than the copy. Below the crossover, two fixed-width moves do it
        // inline; above it, `memcpy` is still the right answer.
        let len = dst.len();
        if len <= 2 * VEC {
            // SAFETY: the slices are distinct (`&mut` and `&` cannot alias),
            // both are exactly `len` bytes by the assertion above, and
            // `copy_small_disjoint` writes only within `0..len` of each.
            unsafe {
                copy_small_disjoint(dst.as_mut_ptr(), src.as_ptr(), len);
            }
            return;
        }
        dst.copy_from_slice(src);
    }

    #[inline(always)]
    fn copy_match(buf: &mut [u8], pos: usize, offset: usize, len: usize) {
        debug_assert!(offset >= 1);
        debug_assert!(offset <= pos);
        debug_assert!(pos + len <= buf.len());

        if len == 0 {
            return;
        }

        // SAFETY: the contract above, asserted in debug and established by
        // `zalt_codec::lz::copy_match` in release, guarantees every access in
        // both kernels is in bounds. Each kernel documents the specific
        // property it relies on.
        unsafe {
            // The short non-overlapping match, handled here rather than three
            // frames down. `copy_match_wide` already has this fast path, but
            // reaching it costs a call, and the disassembly showed two such
            // calls per sequence in the executor — for matches that are
            // typically a handful of bytes, so the call is most of the work.
            //
            // The condition is the wide kernel's own: `offset >= len` means the
            // source cannot overlap the destination, and `len <= 2 * VEC` is
            // what head-and-tail covers in two fixed-width moves.
            if offset >= len && len <= 2 * VEC {
                copy_small(buf.as_mut_ptr(), pos, pos - offset, len);
                return;
            }
            if offset >= VEC {
                copy_match_wide(buf, pos, offset, len);
            } else {
                copy_match_narrow(buf, pos, offset, len);
            }
        }
    }

    /// The first word scalar, then sixteen bytes a step.
    ///
    /// **Why not NEON from the first byte.** Most calls end inside the first
    /// eight bytes — they are candidates sharing a hash and little else — and
    /// there a vector compare and its mask extraction cost more than one XOR:
    /// measured (2026-10-12, M1, one process, variants interleaved) pure NEON
    /// took 1.06–1.25x the scalar loop's time at lengths 3 and 7. Past the
    /// first word the vector loop wins outright: 0.57x at 64 bytes, 0.39x at
    /// 4000, against a best safe 32-byte loop's 0.63x and 0.51x. This is that
    /// hybrid: 0.99–1.00x at the short lengths, the vector loop's speed at the
    /// long ones.
    ///
    /// **The mask.** `vceqq_u8` gives 0xFF per equal byte; inverted, a
    /// non-zero byte marks a difference. `vshrn_n_u16::<4>` narrows each
    /// 16-bit pair to one byte holding four bits from each input byte, so the
    /// 128-bit compare becomes a 64-bit mask with four bits per byte, in order
    /// — the first differing byte is `trailing_zeros / 4`. No `movemask` on
    /// aarch64; this is the usual substitute, and libzstd's.
    #[inline]
    fn common_prefix(a: &[u8], b: &[u8]) -> usize {
        let n = a.len().min(b.len());
        let (Some(x), Some(y)) = (a.first_chunk::<8>(), b.first_chunk::<8>()) else {
            return zalt_codec::Scalar::common_prefix(a, b);
        };
        let diff = u64::from_le_bytes(*x) ^ u64::from_le_bytes(*y);
        if diff != 0 {
            return (diff.trailing_zeros() >> 3) as usize;
        }

        let mut matched = 8usize;
        while matched + VEC <= n {
            // SAFETY: `matched + VEC <= n`, and `n` is the shorter length, so
            // both sixteen-byte loads lie inside their slices. Nothing else
            // here touches memory.
            let mask = unsafe {
                let x = vld1q_u8(a.as_ptr().add(matched));
                let y = vld1q_u8(b.as_ptr().add(matched));
                let differ = vmvnq_u8(vceqq_u8(x, y));
                vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(vreinterpretq_u16_u8(
                    differ,
                ))))
            };
            if mask != 0 {
                return matched + (mask.trailing_zeros() >> 2) as usize;
            }
            matched += VEC;
        }
        // Fewer than sixteen bytes left on the shorter side.
        matched + zalt_codec::Scalar::common_prefix(&a[matched..n], &b[matched..n])
    }
}

/// Forward copy of `len` non-overlapping bytes within one buffer, for
/// `len <= 32`, writing nothing outside `dst..dst + len`.
///
/// Each branch performs two moves of a fixed width, the second anchored to the
/// *end* of the range. Where they overlap they write identical bytes, so the
/// result is the same as a byte-by-byte copy — and because both are fixed
/// width, each is a single load and store rather than a call to a routine that
/// must first branch on a length it cannot see.
///
/// # Safety
///
/// `dst..dst + len` and `src..src + len` must both be within the allocation
/// `base` points into, must not overlap, and `len` must be at most `2 * VEC`.
#[inline]
unsafe fn copy_small(base: *mut u8, dst: usize, src: usize, len: usize) {
    debug_assert!(len <= 2 * VEC);
    unsafe {
        let s = base.add(src);
        let d = base.add(dst);
        if len >= VEC {
            // 16..=32
            let a = vld1q_u8(s);
            let b = vld1q_u8(s.add(len - VEC));
            vst1q_u8(d, a);
            vst1q_u8(d.add(len - VEC), b);
        } else if len >= 8 {
            let a = (s as *const u64).read_unaligned();
            let b = (s.add(len - 8) as *const u64).read_unaligned();
            (d as *mut u64).write_unaligned(a);
            (d.add(len - 8) as *mut u64).write_unaligned(b);
        } else if len >= 4 {
            let a = (s as *const u32).read_unaligned();
            let b = (s.add(len - 4) as *const u32).read_unaligned();
            (d as *mut u32).write_unaligned(a);
            (d.add(len - 4) as *mut u32).write_unaligned(b);
        } else if len >= 2 {
            let a = (s as *const u16).read_unaligned();
            let b = (s.add(len - 2) as *const u16).read_unaligned();
            (d as *mut u16).write_unaligned(a);
            (d.add(len - 2) as *mut u16).write_unaligned(b);
        } else if len == 1 {
            d.write(s.read());
        }
    }
}

/// Copy `len <= 32` bytes between two **non-overlapping** buffers, writing
/// nothing outside `0..len` of either.
///
/// The head-and-tail construction of [`copy_small`], for the case where source
/// and destination are separate allocations rather than one buffer.
///
/// # Safety
///
/// `dst` and `src` must each be valid for `len` bytes, must not overlap, and
/// `len` must be at most `2 * VEC`.
#[inline]
unsafe fn copy_small_disjoint(dst: *mut u8, src: *const u8, len: usize) {
    debug_assert!(len <= 2 * VEC);
    unsafe {
        if len >= VEC {
            let a = vld1q_u8(src);
            let b = vld1q_u8(src.add(len - VEC));
            vst1q_u8(dst, a);
            vst1q_u8(dst.add(len - VEC), b);
        } else if len >= 8 {
            let a = (src as *const u64).read_unaligned();
            let b = (src.add(len - 8) as *const u64).read_unaligned();
            (dst as *mut u64).write_unaligned(a);
            (dst.add(len - 8) as *mut u64).write_unaligned(b);
        } else if len >= 4 {
            let a = (src as *const u32).read_unaligned();
            let b = (src.add(len - 4) as *const u32).read_unaligned();
            (dst as *mut u32).write_unaligned(a);
            (dst.add(len - 4) as *mut u32).write_unaligned(b);
        } else if len >= 2 {
            let a = (src as *const u16).read_unaligned();
            let b = (src.add(len - 2) as *const u16).read_unaligned();
            (dst as *mut u16).write_unaligned(a);
            (dst.add(len - 2) as *mut u16).write_unaligned(b);
        } else if len == 1 {
            dst.write(src.read());
        }
    }
}

/// Shuffle indices for the small-offset match copy, precomputed per offset.
///
/// `PATTERN_IDX[o][j] == j % o` builds the repeating pattern from a single
/// load; `ROTATE_IDX[o][j] == (j + 16 % o) % o` advances that pattern by one
/// vector. Both are pure functions of the offset, and the offset is one of
/// fifteen values.
///
/// They were computed on entry — thirty-two integer divisions before a single
/// byte moved. Real matches are short, so that setup dominated the copy it was
/// setting up. Two 240-byte tables in `.rodata` remove it.
const fn shuffle_tables() -> ([[u8; VEC]; VEC], [[u8; VEC]; VEC]) {
    let mut pattern = [[0u8; VEC]; VEC];
    let mut rotate = [[0u8; VEC]; VEC];
    let mut offset = 1;
    while offset < VEC {
        let step = VEC % offset;
        let mut j = 0;
        while j < VEC {
            pattern[offset][j] = (j % offset) as u8;
            rotate[offset][j] = ((j + step) % offset) as u8;
            j += 1;
        }
        offset += 1;
    }
    (pattern, rotate)
}

/// Index 0 is unused: `copy_match_narrow` is only reached with `offset >= 1`.
const SHUFFLE: ([[u8; VEC]; VEC], [[u8; VEC]; VEC]) = shuffle_tables();

/// Match copy for `offset >= 16`.
///
/// # Chunk size is bounded by the offset
///
/// A `k`-byte chunked copy is only correct when `k <= offset`. Otherwise the
/// load at `d - offset` reaches past `d` into bytes this same call has not
/// written yet, and the periodic semantics are silently lost. So the loop width
/// is tiered against `offset` rather than fixed.
///
/// # Why the widest case is delegated
///
/// When `offset >= len` there is no overlap at all and the operation is a plain
/// forward copy. Benchmarking a flat 16-byte intrinsic loop against the scalar
/// version showed the *scalar* code winning by ~40% on bulk shapes: LLVM
/// auto-vectorises and unrolls it, producing wider accesses than a
/// one-vector-per-iteration loop. `copy_within` lowers to the platform
/// `memmove`, which is better tuned than either.
///
/// The lesson generalises: hand-written NEON earns its place where the compiler
/// *cannot* help — the loop-carried dependency of a short overlapping offset —
/// and loses where it already does well. See `doc/ZALT-MAC.md` §5.
///
/// # Safety
///
/// Requires `offset >= 16`, `offset <= pos`, and `pos + len <= buf.len()`.
#[inline]
unsafe fn copy_match_wide(buf: &mut [u8], pos: usize, offset: usize, len: usize) {
    debug_assert!(offset >= VEC);

    // No overlap at all: the operation is a plain forward copy.
    if offset >= len {
        let src = pos - offset;
        // `copy_within` lowers to a `memmove` *call*, and real matches are
        // short — the profile showed this single line as the largest consumer
        // of `memmove` in the whole decoder, because a six-byte match was
        // paying a libc dispatch to move six bytes. Below the point where a
        // tuned bulk copy earns its call, do it inline.
        //
        // The trick is head-and-tail: two possibly-overlapping fixed-width
        // moves cover any length between the width and twice it, so a handful
        // of widths covers everything without a byte loop and **without
        // overrunning** — no slack is required past `pos + len`, which matters
        // because the window sizes its buffer to exactly that.
        unsafe {
            if len <= 2 * VEC {
                copy_small(buf.as_mut_ptr(), pos, src, len);
                return;
            }
        }
        buf.copy_within(src..src + len, pos);
        return;
    }

    let end = pos + len;
    let base = buf.as_mut_ptr();
    let mut d = pos;

    // Four vectors per iteration. Valid only while the 64-byte load window
    // stays behind `d`, hence the `offset >= 4 * VEC` guard.
    if offset >= 4 * VEC {
        while d + 4 * VEC <= end {
            unsafe {
                let s = base.add(d - offset);
                let v0 = vld1q_u8(s);
                let v1 = vld1q_u8(s.add(VEC));
                let v2 = vld1q_u8(s.add(2 * VEC));
                let v3 = vld1q_u8(s.add(3 * VEC));
                let t = base.add(d);
                vst1q_u8(t, v0);
                vst1q_u8(t.add(VEC), v1);
                vst1q_u8(t.add(2 * VEC), v2);
                vst1q_u8(t.add(3 * VEC), v3);
            }
            d += 4 * VEC;
        }
    }

    while d + VEC <= end {
        unsafe {
            let v = vld1q_u8(base.add(d - offset));
            vst1q_u8(base.add(d), v);
        }
        d += VEC;
    }
    while d < end {
        unsafe { *base.add(d) = *base.add(d - offset) };
        d += 1;
    }
}

/// Match copy for `offset < 16` — the overlapping case.
///
/// This is the kernel that matters. Short offsets are common in real markup
/// (indentation runs, repeated tags) and they defeat naive vector copy, because
/// a 16-byte load from `pos - offset` reads bytes this very call has not written
/// yet. The usual answer is to fall back to a byte loop and eat the cost.
///
/// Instead, build the `offset`-periodic pattern once in a register and rotate
/// it. Let `P[j] = src[j % offset]` for `j` in `0..16`, built with one table
/// lookup. Storing `P` emits the first 16 bytes correctly. The next 16 bytes
/// need `src[(j + 16) % offset]`, which is `P` permuted by
/// `R[j] = (j + 16 % offset) % offset` — and the permuted register is *still*
/// `offset`-periodic, so the same `R` advances every subsequent chunk. One
/// `tbl` per 16 bytes output, no branches, no byte loop.
///
/// # Safety
///
/// Requires `offset >= 1`, `offset <= pos`, `pos + len <= buf.len()`, and that
/// the initial 16-byte load from `pos - offset` stays in bounds. The caller
/// checks the last of these and falls back when it does not hold.
#[inline]
unsafe fn copy_match_narrow(buf: &mut [u8], pos: usize, offset: usize, len: usize) {
    debug_assert!(offset < VEC);

    let end = pos + len;

    // The pattern is built by loading VEC bytes from `pos - offset` and
    // selecting only indices < offset. The unselected lanes may be garbage, but
    // they must still be *readable*. When they are not, there is no room to
    // vectorise anyway: fall back.
    if pos - offset + VEC > buf.len() {
        let base = buf.as_mut_ptr();
        let mut d = pos;
        while d < end {
            unsafe { *base.add(d) = *base.add(d - offset) };
            d += 1;
        }
        return;
    }

    let base = buf.as_mut_ptr();

    let (mut pattern, rot_v): (uint8x16_t, uint8x16_t) = unsafe {
        let raw = vld1q_u8(base.add(pos - offset));
        (
            vqtbl1q_u8(raw, vld1q_u8(SHUFFLE.0[offset].as_ptr())),
            vld1q_u8(SHUFFLE.1[offset].as_ptr()),
        )
    };

    let mut d = pos;
    while d + VEC <= end {
        unsafe {
            vst1q_u8(base.add(d), pattern);
            pattern = vqtbl1q_u8(pattern, rot_v);
        }
        d += VEC;
    }

    // Tail, and for a match shorter than one vector the *whole* copy — which
    // is the common case, since real matches are short. `pattern` already
    // holds the next sixteen bytes of the periodic sequence, so spilling it to
    // the stack and taking a fixed-width prefix beats stepping a byte at a
    // time. It also cannot overrun the destination: `copy_small_disjoint`
    // writes exactly `remaining` bytes, and bytes past `end` are live history
    // in a wrapped ring.
    let remaining = end - d;
    if remaining > 0 {
        let mut staged = [0u8; VEC];
        unsafe {
            vst1q_u8(staged.as_mut_ptr(), pattern);
            copy_small_disjoint(base.add(d), staged.as_ptr(), remaining);
        }
    }
}
