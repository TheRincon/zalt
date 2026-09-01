//! The decoding window: bounded match history.
//!
//! # What the window is for
//!
//! LZ77 matches reach backwards into already-decoded output, so a decoder must
//! retain the last `window_size` bytes. Before this module existed, that history
//! *was* the caller's output buffer — correct, but it forced the caller to hold
//! an entire frame in memory, because dropping any of it would break a later
//! match. A 500 MB response could not be streamed to disk.
//!
//! Owning the history here decouples the two: the decoder keeps a bounded
//! window, and the caller may drain and discard output as fast as it likes.
//!
//! # Ring, not sliding-with-compaction
//!
//! The obvious alternative keeps history in a linear buffer and periodically
//! memmoves the tail to the front. That has one real advantage — history is
//! always contiguous, so the accelerated kernels always apply — but it costs
//! write amplification of `window_size / slack`. With an 8 MiB window and a
//! 128 KiB slack that is 64x, which is unacceptable; making it cheap means
//! roughly doubling the memory.
//!
//! A ring pays neither: exactly `window_size` bytes, no amplification. Its cost
//! is that a match spanning the wrap point cannot be one flat memory operation.
//!
//! That cost is well placed for this workload. **A frame smaller than its window
//! never wraps at all**, so it stays on the flat fast path that uses the
//! [`Backend`] kernels — and small pushed resources are the dominant case. Only
//! frames that exceed their own window reach the slow path.
//!
//! # Lazy allocation
//!
//! Capacity is a ceiling, not a reservation: the buffer grows as bytes are
//! actually written. A frame declaring an 8 MiB window but producing 4 KB
//! allocates 4 KB. This is a security property (§6 — a tiny header must not
//! trigger a large allocation) and, given many small resources decoding
//! concurrently, a performance one: touching 8 MiB of fresh pages per response
//! would be a page-fault storm for data never read.

use alloc::vec::Vec;

use crate::backend::Backend;
use crate::error::{Error, Result};
use crate::lz;

/// Bounded LZ77 match history.
#[derive(Debug, Clone, Default)]
pub struct Window {
    buf: Vec<u8>,
    capacity: usize,
    /// Write cursor. Meaningful only once `buf.len() == capacity`; before that
    /// it tracks `buf.len()`.
    pos: usize,
    total: u64,
}

impl Window {
    /// Create a window bounding history to `capacity` bytes.
    ///
    /// Allocates nothing: capacity is a ceiling that growth approaches.
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: Vec::new(),
            // A zero-capacity window would make the wrap arithmetic divide by
            // zero. Frames declaring no window produce no matches, so one byte
            // of slack costs nothing and removes a special case.
            capacity: capacity.max(1),
            pos: 0,
            total: 0,
        }
    }

    /// Re-arm for a new frame with a new ceiling, keeping the allocation.
    ///
    /// Reuse is the point: a pooled decoder should not re-allocate per response.
    /// The buffer is truncated rather than freed, so a later frame with a
    /// similar window costs no allocation at all.
    pub fn reset(&mut self, capacity: usize) {
        self.buf.clear();
        self.capacity = capacity.max(1);
        self.pos = 0;
        self.total = 0;
    }

    /// Ceiling on retained history.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Bytes of history currently addressable by a match.
    pub fn history_len(&self) -> usize {
        self.buf.len()
    }

    /// Total bytes ever written, across wraps.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Bytes of heap currently held.
    pub fn footprint(&self) -> usize {
        self.buf.capacity()
    }

    /// Surrender the backing store, leaving the window empty.
    pub fn take_buffer(&mut self) -> Vec<u8> {
        self.pos = 0;
        self.total = 0;
        core::mem::take(&mut self.buf)
    }

    /// Adopt a caller-supplied backing store, keeping whichever is larger.
    ///
    /// The donated buffer is cleared: capacity is the gift, contents are not.
    pub fn donate_buffer(&mut self, mut buf: Vec<u8>) {
        buf.clear();
        if buf.capacity() > self.buf.capacity() {
            self.buf = buf;
            self.pos = 0;
            self.total = 0;
        }
    }

    /// Whether the buffer has reached its ceiling and is now cycling.
    #[inline]
    fn is_full(&self) -> bool {
        self.buf.len() == self.capacity
    }

    /// Append bytes, discarding whatever falls out of the window.
    pub fn write_all<B: Backend>(&mut self, src: &[u8]) {
        self.total = self.total.wrapping_add(src.len() as u64);

        // Writing more than the whole window in one go: only the tail can
        // survive, so skip the work of writing bytes that are immediately
        // overwritten.
        let src = if src.len() > self.capacity {
            &src[src.len() - self.capacity..]
        } else {
            src
        };

        // Fast path: the ring is cycling and this run does not reach the wrap
        // point. That is nearly every literal run in a frame larger than its
        // window, and the general loop below re-derives `is_full` and the
        // remaining room on every pass to handle a case that almost never
        // arises. Routing the copy through the backend matters as much as
        // skipping the loop: `copy_from_slice` lowers to a `memcpy` *call*,
        // and literal runs are a handful of bytes.
        if self.is_full() {
            let room = self.capacity - self.pos;
            if src.len() <= room {
                let end = self.pos + src.len();
                B::copy_literals(&mut self.buf[self.pos..end], src);
                self.pos = if end == self.capacity { 0 } else { end };
                return;
            }
        }

        let mut rest = src;
        while !rest.is_empty() {
            if !self.is_full() {
                let room = self.capacity - self.buf.len();
                let take = room.min(rest.len());
                self.buf.extend_from_slice(&rest[..take]);
                rest = &rest[take..];
                self.pos = if self.is_full() { 0 } else { self.buf.len() };
                continue;
            }
            let room = self.capacity - self.pos;
            let take = room.min(rest.len());
            self.buf[self.pos..self.pos + take].copy_from_slice(&rest[..take]);
            self.pos += take;
            if self.pos == self.capacity {
                self.pos = 0;
            }
            rest = &rest[take..];
        }
    }

    /// Byte `distance` back from the write cursor.
    ///
    /// Retained only as the oracle for [`Window::copy_match`]. This and
    /// [`Window::push_byte`] *were* the wrapping path, and keeping them is the
    /// same discipline that keeps the scalar backend alive: the flat-run
    /// version is index arithmetic that is either exactly right or silently
    /// corrupting, and the obvious implementation is the only thing that can
    /// prove which.
    #[cfg(test)]
    #[inline]
    fn byte_back(&self, distance: usize) -> Result<u8> {
        let n = self.buf.len();
        if distance == 0 || distance > n {
            return Err(Error::OffsetTooLarge {
                offset: distance as u64,
                available: n as u64,
            });
        }
        let index = if self.is_full() {
            (self.pos + n - distance) % n
        } else {
            n - distance
        };
        Ok(self.buf[index])
    }

    /// Execute an LZ77 match: copy `len` bytes from `offset` back.
    ///
    /// Byte-at-a-time semantics — when `offset < len` the copy is the
    /// `offset`-periodic extension of the preceding bytes, not a `memmove`.
    pub fn copy_match<B: Backend>(&mut self, offset: usize, len: usize) -> Result<()> {
        if offset == 0 || offset > self.buf.len() {
            return Err(Error::OffsetTooLarge {
                offset: offset as u64,
                available: self.buf.len() as u64,
            });
        }
        if len == 0 {
            return Ok(());
        }

        let mut remaining = len;

        // Phase 1 — the buffer has not yet reached its ceiling, so history is
        // one flat run and appending is a straight in-buffer copy. A frame
        // smaller than its window never leaves this phase.
        if !self.is_full() {
            let take = (self.capacity - self.buf.len()).min(remaining);
            let pos = self.buf.len();
            self.buf.resize(pos + take, 0);
            lz::copy_match::<B>(&mut self.buf, pos, offset, take, 0)?;
            self.pos = if self.is_full() { 0 } else { self.buf.len() };
            remaining -= take;
            if remaining == 0 {
                self.total = self.total.wrapping_add(len as u64);
                return Ok(());
            }
        }

        // Phase 2 — the ring is cycling. Walk it in flat runs.
        //
        // This was byte-at-a-time, with a `%` — an integer division — per byte,
        // and it was justified on the grounds that only frames exceeding their
        // own window reach it. That reasoning holds for the target workload of
        // small pushed resources, and collapses for anything larger: a 34 MB
        // response with the 2 MB window its encoder chose spends 94% of its
        // life here, and never once reaches an accelerated kernel. It was
        // half of all decode time.
        //
        // Splitting a wrapping match into flat runs is the index arithmetic the
        // previous comment here called "the kind that produces silent
        // corruption", which is fair — so the three cases are enumerated
        // explicitly below, and `wrapping_matches_the_byte_at_a_time_reference`
        // checks every one of them against the naive implementation this
        // replaced.
        let cap = self.capacity;
        while remaining > 0 {
            let dst = self.pos;

            let run = if offset == cap {
                // The source *is* the destination: byte `i` reads the byte
                // written `cap` bytes ago, which is the one already sitting at
                // the cursor. Every byte copies onto itself, so only the cursor
                // moves. Left to the general case this would be a correct but
                // pointless self-copy; it is called out because `cap - offset`
                // is zero here and would otherwise bound a run to nothing.
                remaining.min(cap - dst)
            } else if dst >= offset {
                // Source is flat and *behind* the cursor, so `dst - offset` is
                // a real index and the run is exactly the kernel's shape —
                // overlap included, since it owns the periodic semantics.
                let run = remaining.min(cap - dst);
                lz::copy_match::<B>(&mut self.buf, dst, offset, run, 0)?;
                run
            } else {
                // Source is *ahead* in memory: it wrapped, and lives near the
                // end of the buffer while the cursor is near the start. Bound
                // the run so it can neither run off the end (`offset - dst`
                // bytes remain before the source wraps) nor read what this same
                // copy is writing (`cap - offset` bytes separate them). Within
                // those bounds the regions are disjoint and it is a plain move.
                let src = dst + cap - offset;
                let run = remaining.min(offset - dst).min(cap - offset);
                self.buf.copy_within(src..src + run, dst);
                run
            };

            debug_assert!(run > 0, "a zero-length run would not terminate");
            // `run` is bounded by `cap - dst`, so the sum lands in `0..=cap`
            // and the only wrap possible is the exact one. A `%` here would be
            // an integer division per match for a case a compare settles —
            // and `cap` is a window size, which need not be a power of two.
            let next = dst + run;
            self.pos = if next == cap { 0 } else { next };
            remaining -= run;
        }

        self.total = self.total.wrapping_add(len as u64);
        Ok(())
    }

    /// Append one byte. The oracle's other half; see [`Window::byte_back`].
    #[cfg(test)]
    #[inline]
    fn push_byte(&mut self, byte: u8) {
        if !self.is_full() {
            self.buf.push(byte);
            self.pos = if self.is_full() { 0 } else { self.buf.len() };
        } else {
            self.buf[self.pos] = byte;
            self.pos += 1;
            if self.pos == self.capacity {
                self.pos = 0;
            }
        }
        self.total = self.total.wrapping_add(1);
    }

    /// The most recently written `n` bytes, in write order.
    ///
    /// Returned as up to two slices because the range may straddle the wrap
    /// point. The second is empty whenever the range is contiguous.
    pub fn recent(&self, n: usize) -> Result<(&[u8], &[u8])> {
        let len = self.buf.len();
        if n > len {
            return Err(Error::Malformed);
        }
        if n == 0 {
            return Ok((&[], &[]));
        }
        if !self.is_full() {
            return Ok((&self.buf[len - n..], &[]));
        }
        let start = (self.pos + len - n) % len;
        if start + n <= len {
            Ok((&self.buf[start..start + n], &[]))
        } else {
            Ok((&self.buf[start..], &self.buf[..n - (len - start)]))
        }
    }
}

#[cfg(test)]
mod tests {

    // ------------------------------------------------- the wrapping rewrite

    /// The byte-at-a-time wrapping path this replaced, verbatim.
    fn reference_copy_match(w: &mut Window, offset: usize, len: usize) -> Result<()> {
        for _ in 0..len {
            let byte = w.byte_back(offset)?;
            w.push_byte(byte);
        }
        Ok(())
    }

    fn filled(capacity: usize, written: usize) -> Window {
        let mut w = Window::new(capacity);
        let data: Vec<u8> = (0..written).map(|i| (i % 251) as u8).collect();
        w.write_all::<Scalar>(&data);
        w
    }

    /// The test the flat-run rewrite rests on.
    ///
    /// A wrapping match can straddle the ring boundary on the source side, the
    /// destination side, or both, and can overlap its own output. The rewrite
    /// enumerates three cases and bounds each run by hand; any one of those
    /// bounds being off by one produces plausible-looking output rather than a
    /// crash. Comparing against the implementation it replaced, over every
    /// combination that fits, is the only check that actually settles it.
    #[test]
    fn wrapping_matches_the_byte_at_a_time_reference() {
        // See the note in `bits.rs`: the full cross-product is far too slow
        // under Miri, and the three wrapping cases are all reachable with the
        // small capacities.
        #[cfg(miri)]
        let capacities = &[1usize, 3, 8][..];
        #[cfg(not(miri))]
        let capacities = &[1usize, 2, 3, 5, 8, 16, 17, 32][..];

        for &capacity in capacities {
            // `written` past `capacity` puts the cursor at every phase of the
            // ring, which is what decides whether source or destination wraps.
            for written in 0..=capacity * 2 + 3 {
                for offset in 1..=capacity {
                    for len in 1..=capacity * 2 + 2 {
                        let mut fast = filled(capacity, written);
                        let mut slow = filled(capacity, written);

                        // Only compare where the offset is addressable at all;
                        // the rejection path is covered separately.
                        if offset > fast.buf.len() {
                            continue;
                        }

                        let fast_result = fast.copy_match::<Scalar>(offset, len);
                        let slow_result = reference_copy_match(&mut slow, offset, len);

                        assert_eq!(
                            fast_result.is_ok(),
                            slow_result.is_ok(),
                            "cap={capacity} written={written} offset={offset} len={len}"
                        );
                        if fast_result.is_err() {
                            continue;
                        }
                        assert_eq!(
                            fast.buf, slow.buf,
                            "contents: cap={capacity} written={written} offset={offset} len={len}"
                        );
                        assert_eq!(
                            fast.pos, slow.pos,
                            "cursor: cap={capacity} written={written} offset={offset} len={len}"
                        );
                        assert_eq!(
                            fast.total, slow.total,
                            "total: cap={capacity} written={written} offset={offset} len={len}"
                        );
                    }
                }
            }
        }
    }

    /// The degenerate case the rewrite calls out by name: an offset equal to
    /// the whole window reads the byte sitting under the cursor, so every byte
    /// copies onto itself and only the cursor moves.
    #[test]
    fn an_offset_of_the_whole_window_is_a_cursor_move() {
        let mut w = filled(16, 16);
        let before = w.buf.clone();
        w.copy_match::<Scalar>(16, 40).unwrap();
        assert_eq!(w.buf, before, "the buffer must be untouched");
        assert_eq!(w.pos, 40 % 16);
    }

    /// A match that begins before the ring is full and continues past the
    /// point where it fills — the hand-off between the two phases.
    #[test]
    fn a_match_spanning_the_fill_boundary_is_correct() {
        #[cfg(miri)]
        let capacities = &[8usize][..];
        #[cfg(not(miri))]
        let capacities = &[8usize, 16, 64][..];
        for &capacity in capacities {
            for written in 1..capacity {
                for offset in 1..=written {
                    let len = capacity * 2;
                    let mut fast = filled(capacity, written);
                    let mut slow = filled(capacity, written);
                    fast.copy_match::<Scalar>(offset, len).unwrap();
                    reference_copy_match(&mut slow, offset, len).unwrap();
                    assert_eq!(
                        fast.buf, slow.buf,
                        "cap={capacity} written={written} offset={offset}"
                    );
                    assert_eq!(fast.pos, slow.pos);
                }
            }
        }
    }
    use super::*;
    use crate::backend::Scalar;
    use alloc::vec::Vec;

    /// Read the whole window back in write order.
    fn contents(w: &Window) -> Vec<u8> {
        let (a, b) = w.recent(w.history_len()).unwrap();
        let mut v = Vec::from(a);
        v.extend_from_slice(b);
        v
    }

    #[test]
    fn grows_lazily_rather_than_reserving() {
        let mut w = Window::new(1 << 20);
        assert_eq!(w.footprint(), 0, "a 1 MiB window must allocate nothing");
        w.write_all::<Scalar>(b"abc");
        assert!(
            w.footprint() < 1024,
            "writing 3 bytes must not allocate the ceiling, got {}",
            w.footprint()
        );
        assert_eq!(w.history_len(), 3);
    }

    #[test]
    fn keeps_only_the_most_recent_bytes() {
        let mut w = Window::new(4);
        w.write_all::<Scalar>(b"abcdefg");
        assert_eq!(w.history_len(), 4);
        assert_eq!(contents(&w), b"defg");
        assert_eq!(w.total(), 7);
    }

    #[test]
    fn a_single_write_larger_than_the_window_keeps_the_tail() {
        let mut w = Window::new(3);
        w.write_all::<Scalar>(b"0123456789");
        assert_eq!(contents(&w), b"789");
        assert_eq!(w.total(), 10);
    }

    #[test]
    fn repeated_writes_cycle_correctly() {
        let mut w = Window::new(5);
        for chunk in [&b"ab"[..], b"cde", b"fg", b"hij"] {
            w.write_all::<Scalar>(chunk);
        }
        // abcdefghij -> last five
        assert_eq!(contents(&w), b"fghij");
    }

    #[test]
    fn match_within_the_flat_fast_path() {
        let mut w = Window::new(64);
        w.write_all::<Scalar>(b"abcd");
        w.copy_match::<Scalar>(4, 4).unwrap();
        assert_eq!(contents(&w), b"abcdabcd");
    }

    #[test]
    fn overlapping_match_repeats_the_pattern() {
        let mut w = Window::new(64);
        w.write_all::<Scalar>(b"x");
        w.copy_match::<Scalar>(1, 7).unwrap();
        assert_eq!(contents(&w), b"xxxxxxxx");
    }

    /// The case the ring exists for: a match executed while the window is
    /// already cycling, reading across the wrap point.
    #[test]
    fn match_across_the_wrap_point() {
        let mut w = Window::new(8);
        w.write_all::<Scalar>(b"ABCDEFGH");
        assert!(w.is_full());
        w.write_all::<Scalar>(b"ij");
        // window now holds CDEFGHij
        assert_eq!(contents(&w), b"CDEFGHij");
        // Copy 4 bytes from 6 back: "EFGH".
        w.copy_match::<Scalar>(6, 4).unwrap();
        assert_eq!(contents(&w), b"GHijEFGH");
    }

    #[test]
    fn overlapping_match_while_wrapped() {
        let mut w = Window::new(6);
        w.write_all::<Scalar>(b"abcdef");
        // Periodic extension at offset 2 yields "efefe", so the full stream is
        // "abcdefefefe" and the window retains its last six bytes.
        w.copy_match::<Scalar>(2, 5).unwrap();
        assert_eq!(w.total(), 11);
        assert_eq!(contents(&w), b"fefefe");
    }

    #[test]
    fn offset_past_history_is_rejected() {
        let mut w = Window::new(16);
        w.write_all::<Scalar>(b"abc");
        assert!(matches!(
            w.copy_match::<Scalar>(4, 1),
            Err(Error::OffsetTooLarge { .. })
        ));
        assert!(w.copy_match::<Scalar>(0, 1).is_err());
    }

    /// A match may reach back only into retained history, even though earlier
    /// bytes existed. Forgetting this reads whatever the ring happens to hold.
    #[test]
    fn offset_beyond_the_window_is_rejected_even_though_bytes_existed() {
        let mut w = Window::new(4);
        w.write_all::<Scalar>(b"abcdefgh");
        assert_eq!(w.total(), 8);
        assert!(
            w.copy_match::<Scalar>(5, 1).is_err(),
            "5 bytes back is outside a 4-byte window"
        );
        assert!(w.copy_match::<Scalar>(4, 1).is_ok());
    }

    #[test]
    fn recent_spans_the_wrap_in_two_slices() {
        // Written in two calls on purpose. A single write larger than the
        // window is trimmed to its tail and lands the cursor back at zero, so
        // it never actually straddles — filling first and then overwriting is
        // what puts the cursor mid-buffer.
        let mut w = Window::new(4);
        w.write_all::<Scalar>(b"abcd");
        w.write_all::<Scalar>(b"ef");
        let (a, b) = w.recent(4).unwrap();
        assert_eq!([a, b].concat(), b"cdef");
        assert!(!b.is_empty(), "this layout should straddle the wrap");
    }

    /// The trimming case, stated separately so the distinction is recorded.
    #[test]
    fn oversized_single_write_lands_contiguous() {
        let mut w = Window::new(4);
        w.write_all::<Scalar>(b"abcdef");
        let (a, b) = w.recent(4).unwrap();
        assert_eq!(a, b"cdef");
        assert!(b.is_empty());
    }

    #[test]
    fn recent_more_than_history_is_an_error() {
        let mut w = Window::new(8);
        w.write_all::<Scalar>(b"abc");
        assert!(w.recent(4).is_err());
        assert!(w.recent(3).is_ok());
    }

    #[test]
    fn reset_rearms_without_freeing() {
        let mut w = Window::new(64);
        w.write_all::<Scalar>(b"abcdefgh");
        let before = w.footprint();
        w.reset(32);
        assert_eq!(w.history_len(), 0);
        assert_eq!(w.total(), 0);
        assert_eq!(w.capacity(), 32);
        assert_eq!(w.footprint(), before, "reset must keep the allocation");
    }

    #[test]
    fn zero_capacity_is_survivable() {
        let mut w = Window::new(0);
        w.write_all::<Scalar>(b"");
        assert_eq!(w.history_len(), 0);
        assert!(w.copy_match::<Scalar>(1, 1).is_err());
    }
}
