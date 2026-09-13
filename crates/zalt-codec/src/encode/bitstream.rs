//! The forward bit writer whose output the backward reader consumes.
//!
//! `BIT_CStream` in libzstd. Bits accumulate into a 64-bit container from the
//! **low end up**, complete bytes are flushed little-endian, and the stream is
//! closed with a single `1` sentinel bit above the last real bit.
//!
//! That sentinel is the whole handshake between the two directions.
//! [`crate::bits::BackwardBits::new`] finds the highest set bit of the final
//! byte and starts reading immediately below it, so a stream closed without one
//! is unreadable and a stream closed with two is off by however many bits.
//!
//! ## When the container is flushed, and the measurement that changed it
//!
//! libzstd flushes at carefully chosen points and reasons, in comments counting
//! bits, about how much can accumulate between them. That is a throughput
//! optimisation resting on an invariant a reader has to reconstruct across call
//! sites, and this module originally refused it: every `add_bits` flushed, and
//! the comment here said the change was the right one to make *"when there is a
//! measurement asking for it"*.
//!
//! §10's M5 profile is that measurement. `CState::encode` and
//! `Coded::write_values` — which are almost entirely `add_bits` — were 11% of
//! encode time on a large file and 14% on a small one, and nearly all of it was
//! a flush that pushed bytes **one at a time** through a `Vec` bounds check per
//! byte, for a field that usually completes no byte at all.
//!
//! **The invariant taken is local, which is the whole point.** A field is at
//! most 32 bits — [`BitWriter::add_bits`] asserts it, and the widest field in
//! the format is a 31-bit offset code — so flushing whenever `bit_pos` exceeds
//! 32 leaves at most 32 bits behind, and the next field cannot push the
//! container past 64. That is provable from the assertion three lines above it,
//! not from a bit budget maintained by every caller. The bytes emitted are
//! identical either way: only *when* a complete byte leaves the container
//! changes, never the order bits enter it.

use alloc::vec::Vec;

/// Accumulates bits and emits them little-endian.
pub struct BitWriter {
    out: Vec<u8>,
    container: u64,
    bit_pos: u32,
}

impl BitWriter {
    /// The widest field this writer accepts, in bits.
    ///
    /// Not a tuning constant — it is what the format actually contains. The
    /// widest single field is an offset code, which is the position of the
    /// highest set bit of an `Offset_Value` and so at most 31 for a window the
    /// encoder can declare. Literals-length and match-length extra bits reach
    /// 16, an FSE state transition `tableLog + 1` reaches 10, and a Huffman code
    /// reaches 11.
    const MAX_FIELD_BITS: u32 = 32;

    /// A writer with nothing in it.
    pub fn new() -> Self {
        Self::with_capacity(0)
    }

    /// A writer that will not reallocate for the first `bytes` of output.
    ///
    /// Every caller knows roughly how large its stream will be — a sequence
    /// bitstream is bounded by its sequence count, a Huffman segment by its
    /// literals — and growing a `Vec` from empty doubles its way there through
    /// a chain of copies, per block, in the encoder's hot path.
    pub fn with_capacity(bytes: usize) -> Self {
        Self {
            out: Vec::with_capacity(bytes),
            container: 0,
            bit_pos: 0,
        }
    }

    /// Append the low `nb_bits` of `value`.
    ///
    /// A zero-width write is a no-op rather than a masked zero: the mask
    /// `(1 << 0) - 1` is zero, which happens to be right, but `1u64 << 64` for
    /// a 64-bit field is not, and treating the degenerate case explicitly means
    /// the mask below never has to be correct at both ends of its range.
    pub fn add_bits(&mut self, value: u64, nb_bits: u32) {
        if nb_bits == 0 {
            return;
        }
        debug_assert!(nb_bits <= Self::MAX_FIELD_BITS);
        let mask = (1u64 << nb_bits) - 1;
        self.container |= (value & mask) << self.bit_pos;
        self.bit_pos += nb_bits;
        // The container now holds at most 64 bits, because `bit_pos` was at
        // most `MAX_FIELD_BITS` on entry. Flushing back below that threshold
        // re-establishes it for the next field, and is the only thing any
        // caller has to be able to rely on.
        if self.bit_pos > Self::MAX_FIELD_BITS {
            self.flush();
        }
    }

    /// Write out every complete byte the container holds.
    fn flush(&mut self) {
        let nb_bytes = (self.bit_pos >> 3) as usize;
        // One append of up to eight bytes, rather than a push and a capacity
        // check per byte. `to_le_bytes` is the same little-endian order the
        // loop this replaced produced by shifting.
        self.out
            .extend_from_slice(&self.container.to_le_bytes()[..nb_bytes.min(8)]);
        self.bit_pos &= 7;
        // `>>= 64` is undefined for a u64 and panics in debug. `nb_bytes`
        // reaches 8 exactly when the container is full, and then nothing is
        // left over.
        self.container = if nb_bytes >= 8 {
            0
        } else {
            self.container >> (nb_bytes * 8)
        };
    }

    /// Close the stream, appending the end marker.
    ///
    /// The marker is a `1` bit written above everything else. A partial final
    /// byte is emitted whole, so the reader sees the marker as the highest set
    /// bit of the last byte and knows where the payload stops.
    pub fn close(mut self) -> Vec<u8> {
        self.add_bits(1, 1);
        // Deliberate, and the one place the deferred flush is load-bearing:
        // `add_bits` no longer empties the container, so up to four whole bytes
        // may still be sitting in it. Writing `container as u8` straight out —
        // which was correct when every field flushed — would keep the lowest
        // byte and silently discard the rest of the stream.
        self.flush();
        if self.bit_pos > 0 {
            self.out.push(self.container as u8);
        }
        self.out
    }
}

impl Default for BitWriter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BackwardBits;

    /// Written forward, read backward: the round trip the format is built on.
    ///
    /// The reader consumes fields in the reverse of the order they were
    /// written, which is exactly why the sequence encoder writes sequences from
    /// last to first. Pinning it here means the sequence writer can be reasoned
    /// about as an ordering problem rather than a bit-layout one.
    #[test]
    fn fields_read_back_in_reverse_order() {
        let fields: [(u64, u32); 5] = [(0b101, 3), (0xABCD, 16), (1, 1), (0, 4), (0x3F, 6)];

        let mut w = BitWriter::new();
        for (value, bits) in fields {
            w.add_bits(value, bits);
        }
        let bytes = w.close();

        let mut r = BackwardBits::new(&bytes).expect("a stream we just closed");
        for (value, bits) in fields.iter().rev() {
            assert_eq!(u64::from(r.read(*bits)), *value, "field of {bits} bits");
        }
        assert!(!r.exhausted(), "the reader must not have run past the end");
    }

    #[test]
    fn the_end_marker_is_the_highest_set_bit() {
        // What `BackwardBits::new` keys on. A stream whose last byte is zero
        // has no marker and is unreadable, which is the failure this prevents.
        for bits in 1..=17u32 {
            let mut w = BitWriter::new();
            w.add_bits(0, bits);
            let bytes = w.close();
            let last = *bytes.last().expect("non-empty");
            assert_ne!(last, 0, "{bits} zero bits still needs a marker byte");
            // Everything written was zero, so the marker is the only set bit.
            assert_eq!(last.count_ones(), 1, "{bits} bits");
        }
    }

    #[test]
    fn an_empty_stream_is_still_readable() {
        let bytes = BitWriter::new().close();
        assert_eq!(bytes, [1], "just the marker");
        assert!(BackwardBits::new(&bytes).is_ok());
    }

    #[test]
    fn zero_width_writes_change_nothing() {
        let mut a = BitWriter::new();
        a.add_bits(0xFF, 0);
        a.add_bits(0b11, 2);
        a.add_bits(0xFF, 0);

        let mut b = BitWriter::new();
        b.add_bits(0b11, 2);

        assert_eq!(a.close(), b.close());
    }

    /// Whole bytes may sit in the container when `close` is called, and all of
    /// them have to come out.
    ///
    /// The failure this pins is specific to deferring the flush: `close` used to
    /// write `container as u8`, which was right when every field flushed and
    /// silently drops up to four bytes now that they do not. It is silent
    /// because the end marker still lands and the stream still parses — it just
    /// parses as a shorter one. Checked by removing the flush again, which fails
    /// this and three of the tests around it.
    #[test]
    fn a_container_holding_whole_bytes_is_emptied_by_close() {
        // Every width up to the widest field, so some combination leaves whole
        // bytes behind at close whatever the flush threshold happens to be.
        for width in 1..=32u32 {
            let mut w = BitWriter::new();
            let fields = [0xDEAD_BEEFu64, 0x1234_5678, 0x0F0F_0F0F];
            for value in fields {
                w.add_bits(value, width);
            }
            let bytes = w.close();

            let mut r = BackwardBits::new(&bytes).expect("a stream we just closed");
            let mask = (1u64 << width) - 1;
            for value in fields.iter().rev() {
                assert_eq!(
                    u64::from(r.read(width)),
                    value & mask,
                    "field of {width} bits did not survive close"
                );
            }
            assert!(!r.exhausted(), "{width} bits: reader ran past the end");
        }
    }

    /// The widest field the format contains must not overflow the container.
    ///
    /// `MAX_FIELD_BITS` is the whole of the deferred flush's correctness
    /// argument: `bit_pos` is at most that on entry, so a field of that width
    /// leaves at most 64 bits behind. An offset code of 31 bits is the real
    /// case, and 32 is the asserted ceiling.
    #[test]
    fn back_to_back_maximum_width_fields_round_trip() {
        let mut w = BitWriter::new();
        let fields: [(u64, u32); 6] = [
            (0xFFFF_FFFF, 32),
            (1, 1),
            (0x7FFF_FFFF, 31),
            (0xFFFF, 16),
            (0x3FF, 10),
            (0xFFFF_FFFF, 32),
        ];
        for (value, bits) in fields {
            w.add_bits(value, bits);
        }
        let bytes = w.close();

        let mut r = BackwardBits::new(&bytes).expect("a stream we just closed");
        for (value, bits) in fields.iter().rev() {
            assert_eq!(u64::from(r.read(*bits)), *value, "field of {bits} bits");
        }
        assert!(!r.exhausted(), "the reader must not have run past the end");
    }

    #[test]
    fn values_are_masked_to_their_field_width() {
        // A caller passing a wider value must not corrupt neighbouring fields.
        let mut w = BitWriter::new();
        w.add_bits(0xFFFF_FFFF, 4);
        w.add_bits(0b1010, 4);
        let bytes = w.close();
        let mut r = BackwardBits::new(&bytes).unwrap();
        assert_eq!(r.read(4), 0b1010);
        assert_eq!(r.read(4), 0b1111);
    }
}
