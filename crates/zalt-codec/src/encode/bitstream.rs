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
//! ## Why the container is flushed after every field
//!
//! libzstd flushes at carefully chosen points and reasons, in comments counting
//! bits, about how much can accumulate between them. That is a throughput
//! optimisation resting on an invariant a reader has to reconstruct. Here every
//! `add_bits` is followed by a flush, so `bit_pos` never exceeds 7 + 32 and the
//! container cannot overflow — no invariant, nothing to get wrong, and the same
//! bytes come out. When there is a measurement asking for it, that is the
//! change to make; §10 is emphatic that this is not the moment.

use alloc::vec::Vec;

/// Accumulates bits and emits them little-endian.
pub struct BitWriter {
    out: Vec<u8>,
    container: u64,
    bit_pos: u32,
}

impl BitWriter {
    /// A writer with nothing in it.
    pub fn new() -> Self {
        Self {
            out: Vec::new(),
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
        debug_assert!(nb_bits < 64);
        let mask = (1u64 << nb_bits) - 1;
        self.container |= (value & mask) << self.bit_pos;
        self.bit_pos += nb_bits;
        self.flush();
    }

    /// Write out every complete byte the container holds.
    fn flush(&mut self) {
        let nb_bytes = (self.bit_pos >> 3) as usize;
        for i in 0..nb_bytes {
            self.out.push((self.container >> (8 * i)) as u8);
        }
        self.bit_pos &= 7;
        // `>>= 64` is undefined for a u64 and panics in debug. Flushing after
        // every field keeps `nb_bytes` well under 8, but the guard costs
        // nothing and removes the dependency on that.
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
