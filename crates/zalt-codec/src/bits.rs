//! Bit readers.
//!
//! zstd uses **two opposite conventions**, and confusing them is the single
//! most efficient way to produce a decoder that almost works:
//!
//! - [`ForwardBits`] — little-endian, LSB-first, front to back. Used for FSE
//!   distribution headers and Huffman weight headers.
//! - [`BackwardBits`] — MSB-first, reading from the **last** byte toward the
//!   first. Used for every actual entropy-coded payload: Huffman literal
//!   streams and the sequences bitstream.
//!
//! The backward reader also carries the format's end-of-stream convention: the
//! final byte's highest set bit is a marker, not data, and everything above it
//! is padding.
//!
//! # Correctness before speed
//!
//! Both readers here are the obvious bit-at-a-time formulation. That is
//! deliberate for M1: they are the reference semantics, and M3 replaces the hot
//! paths with word-at-a-time refills measured against them. Optimising a bit
//! reader before there is anything to test it against is how subtle
//! off-by-one-bit bugs get baked in.

use crate::error::{Error, Result};

/// Widest field [`BackwardBits::read_wide`] can take in one call.
///
/// The cursor may sit up to seven bits into the cached 64-bit word, so this is
/// `64 - 7`. A caller wanting more must split the read.
pub const WIDE_MAX: u32 = 57;

/// Forward, little-endian, LSB-first bit reader.
#[derive(Debug, Clone)]
pub struct ForwardBits<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> ForwardBits<'a> {
    /// Create a reader over `data`.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    /// Total bits available.
    pub fn total_bits(&self) -> usize {
        self.data.len() * 8
    }

    /// Bits consumed so far.
    pub fn bits_read(&self) -> usize {
        self.bit_pos
    }

    /// Whole bytes consumed, rounded up. The FSE header format defines its own
    /// length this way.
    pub fn bytes_read(&self) -> usize {
        self.bit_pos.div_ceil(8)
    }

    /// Whether the reader has run past the end of its data.
    pub fn exhausted(&self) -> bool {
        self.bit_pos > self.total_bits()
    }

    #[inline]
    fn bit(&self, index: usize) -> u32 {
        // Past the end reads as zero. Callers that care check `exhausted`;
        // returning zero rather than panicking keeps malformed input on the
        // error path instead of the abort path.
        match self.data.get(index / 8) {
            Some(byte) => u32::from((byte >> (index % 8)) & 1),
            None => 0,
        }
    }

    /// Read `n` bits, first bit read becoming the least significant.
    pub fn read(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        let mut value = 0u32;
        for i in 0..n {
            value |= self.bit(self.bit_pos) << i;
            self.bit_pos += 1;
        }
        value
    }

    /// Peek `n` bits without consuming them.
    pub fn peek(&self, n: u32) -> u32 {
        let mut value = 0u32;
        for i in 0..n {
            value |= self.bit(self.bit_pos + i as usize) << i;
        }
        value
    }

    /// Discard `n` bits.
    pub fn skip(&mut self, n: u32) {
        self.bit_pos += n as usize;
    }

    /// Fail if the reader ran past the end of its data.
    pub fn check(&self) -> Result<()> {
        if self.exhausted() {
            return Err(Error::Malformed);
        }
        Ok(())
    }
}

/// Backward, MSB-first bit reader with zstd's end-of-stream marker convention.
///
/// The stream is read from its last byte toward its first, taking bits from
/// most significant to least within each byte. The highest set bit of the final
/// byte marks where real data begins; that bit and everything above it are
/// padding, not content.
#[derive(Debug, Clone)]
pub struct BackwardBits<'a> {
    data: &'a [u8],
    /// Index into the conceptual reversed bit string, already past the padding.
    bit_pos: usize,
    /// Total readable bits, excluding padding and the marker.
    total_bits: usize,
    /// 64 bits of the stream beginning at `cache_base`, most significant first.
    ///
    /// Entropy decoding consumes a handful of bits per symbol, so re-loading a
    /// word from memory on every `peek` means a load per symbol for data that
    /// was already in a register. Holding the word and refilling only when the
    /// cursor would run off its end costs one predictable branch and turns
    /// roughly six loads into one.
    cache: u64,
    /// Bit index the cache begins at. Always a multiple of eight, and never
    /// ahead of `bit_pos`.
    cache_base: usize,
}

impl<'a> BackwardBits<'a> {
    /// Create a reader, consuming the end-of-stream marker.
    ///
    /// Fails on empty input, and on a final byte of zero — which has no marker
    /// bit and so cannot say where the data ends.
    pub fn new(data: &'a [u8]) -> Result<Self> {
        let last = *data.last().ok_or(Error::Malformed)?;
        if last == 0 {
            // A zero final byte carries no marker. Real encoders never emit
            // this; accepting it would mean guessing where the stream starts.
            return Err(Error::Malformed);
        }
        // Position of the highest set bit, 0-indexed from the LSB.
        let highbit = 7 - last.leading_zeros() as usize;
        // Skip the padding above the marker, plus the marker itself.
        let padding = 8 - highbit;
        Ok(Self::seeded(data, padding, data.len() * 8))
    }

    /// Build a reader positioned at an arbitrary bit, with the cache primed.
    ///
    /// The cache is loaded from bit zero rather than from `bit_pos`, which is
    /// correct for any starting position under eight — the only ones `new`
    /// produces — and is repaired by the first refill for anything larger.
    fn seeded(data: &'a [u8], bit_pos: usize, total_bits: usize) -> Self {
        let mut reader = Self {
            data,
            bit_pos,
            total_bits,
            cache: 0,
            cache_base: 0,
        };
        reader.refill();
        reader
    }

    /// Reload the cache so that it covers the current cursor.
    #[inline]
    fn refill(&mut self) {
        self.cache_base = self.bit_pos - self.bit_pos % 8;
        self.cache = self.word_at(self.bit_pos);
    }

    /// Bits still unread.
    pub fn bits_remaining(&self) -> usize {
        self.total_bits.saturating_sub(self.bit_pos)
    }

    /// Whether every bit has been consumed exactly.
    pub fn is_finished(&self) -> bool {
        self.bit_pos >= self.total_bits
    }

    /// Whether the reader has run past the end of its data.
    pub fn exhausted(&self) -> bool {
        self.bit_pos > self.total_bits
    }

    /// The 64 bits of the conceptual stream beginning at the byte containing
    /// `bit_index`, most significant first.
    ///
    /// # The trick, and why it is not a reinterpretation
    ///
    /// This reader's bit string is `data` walked backwards by byte, MSB-first
    /// within each byte — which is the same thing as the *reversed* byte
    /// sequence read as one big-endian number. Reversing bytes and reading
    /// big-endian is exactly what reading the un-reversed bytes **little-endian**
    /// does, so the eight bytes ending at the current position load as a single
    /// `u64::from_le_bytes` with the wanted bit already at bit 63.
    ///
    /// That turns `read(n)` from an `n`-iteration loop with a bounds-checked
    /// byte load per bit into one load, one shift and one mask.
    /// `#[inline(always)]`, not `#[inline]`.
    ///
    /// This is the load the whole word-at-a-time reader is built around, and
    /// the hint was not taken: it stayed a real call, nine of them inside
    /// `huf::Table::decode_four` alone. Every one spilled the cursor and
    /// reloaded it, which is precisely the state the design exists to keep in
    /// registers — the reference's equivalent loop makes no calls at all.
    #[inline(always)]
    fn word_at(&self, bit_index: usize) -> u64 {
        let len = self.data.len();
        let k = bit_index / 8;

        if len >= k + 8 {
            let start = len - k - 8;
            let bytes: [u8; 8] = self.data[start..start + 8]
                .try_into()
                .expect("the length was just checked");
            return u64::from_le_bytes(bytes);
        }

        // Near the front of the buffer, where fewer than eight bytes remain
        // behind the cursor. The missing bytes lie *past the end* of the
        // conceptual stream and read as zero, matching `bit`. Whole bytes are
        // either entirely in range or entirely out — `total_bits` is a multiple
        // of eight — so there is no partial-byte case to get wrong.
        let mut word = 0u64;
        for j in 0..8usize {
            let idx = k + j;
            if idx < len {
                word |= (self.data[len - 1 - idx] as u64) << (56 - 8 * j);
            }
        }
        word
    }

    /// Read `n` bits, first bit read becoming the most significant.
    ///
    /// Reading zero bits yields zero without touching the stream — which the
    /// sequence decoder relies on, since many symbols carry no extra bits.
    /// `#[inline(always)]` because these are the innermost operations in the
    /// decoder and LLVM would not inline them on its own.
    ///
    /// `huf::Table::decode_four` was making **two calls per symbol decoded** —
    /// eleven call sites to this type in one 512-instruction function — where
    /// the reference's equivalent loop has none at all and is 1307 instructions
    /// of inlined, unrolled code. The same applied to `sequences::decode`. A
    /// call here costs the spill and reload of the bit accumulator and cursor
    /// that the whole word-at-a-time design exists to keep in registers.
    #[inline(always)]
    pub fn read(&mut self, n: u32) -> u32 {
        let value = self.peek(n);
        self.bit_pos += n as usize;
        value
    }

    /// Peek `n` bits without consuming them.
    ///
    /// Huffman decoding needs this: the table is indexed by a full `tableLog`
    /// window, but only the decoded symbol's own bit count is consumed.
    #[inline(always)]
    pub fn peek(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        // How far into the cached word the cursor has advanced. `skip` moves
        // the cursor without touching the cache, so this grows until the
        // request would reach past the word's end.
        let mut off = self.bit_pos - self.cache_base;
        if off + n as usize > 64 {
            self.refill();
            off = self.bit_pos - self.cache_base;
        }
        // After a refill `off` is under eight and `n` is at most 32, so
        // neither shift can reach 64 and neither is undefined.
        ((self.cache << off) >> (64 - n)) as u32
    }

    /// Read up to [`WIDE_MAX`] bits as one field.
    ///
    /// The sequences loop takes offset, match-length and literal-length bits as
    /// three consecutive reads, which together can exceed 32. Reading them as
    /// one field and splitting it removes two of the six bit reads a sequence
    /// performs.
    ///
    /// The ceiling is [`WIDE_MAX`] rather than 64 because the cursor can sit up
    /// to seven bits into the cached word, and `off + n` must stay within it.
    #[inline(always)]
    pub fn read_wide(&mut self, n: u32) -> u64 {
        debug_assert!(n <= WIDE_MAX);
        if n == 0 {
            return 0;
        }
        let mut off = self.bit_pos - self.cache_base;
        if off + n as usize > 64 {
            self.refill();
            off = self.bit_pos - self.cache_base;
        }
        let value = (self.cache << off) >> (64 - n);
        self.bit_pos += n as usize;
        value
    }

    /// Discard `n` bits.
    #[inline(always)]
    pub fn skip(&mut self, n: u32) {
        self.bit_pos += n as usize;
    }

    /// Fail if the reader ran past the end of its data.
    #[inline(always)]
    pub fn check(&self) -> Result<()> {
        if self.exhausted() {
            return Err(Error::Malformed);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    // ------------------------------------------------ word-at-a-time refill

    /// The bit-at-a-time formulation the fast path replaced, kept as the
    /// oracle.
    ///
    /// This is the whole safety net for `word_at`. The fast path is a bit of
    /// index arithmetic that is either exactly right or subtly wrong, and
    /// "subtly wrong" in a bit reader means plausible garbage rather than a
    /// crash — the failure mode §10 already recorded once. Comparing against
    /// the obvious implementation over exhaustive positions is the only test
    /// that actually pins it.
    fn reference_peek(data: &[u8], bit_pos: usize, n: u32) -> u32 {
        let total_bits = data.len() * 8;
        let bit = |index: usize| -> u32 {
            if index >= total_bits {
                return 0;
            }
            let byte = data[data.len() - 1 - index / 8];
            u32::from((byte >> (7 - index % 8)) & 1)
        };
        let mut value = 0u32;
        for i in 0..n as usize {
            value = (value << 1) | bit(bit_pos + i);
        }
        value
    }

    /// A deterministic byte stream with no structure the reader could
    /// accidentally exploit.
    fn pseudorandom(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn backward_word_refill_matches_the_bit_at_a_time_reference() {
        // Miri interprets every operation, so the full sweep is hours there
        // for a property four buffer lengths already establish — the branch
        // being checked is `len >= k + 8`, which turns over well before 33.
        #[cfg(miri)]
        let lengths = &[1usize, 8, 9, 17][..];
        #[cfg(not(miri))]
        let lengths = &[1usize, 2, 3, 7, 8, 9, 15, 16, 17, 33, 64, 129][..];

        for &len in lengths {
            let data = pseudorandom(len, len as u64 * 0x9E37);
            let total_bits = len * 8;
            // Past the end as well: the reader must keep returning zeros there
            // rather than panicking, because that is what puts malformed input
            // on the error path instead of the abort path.
            for bit_pos in 0..total_bits + 24 {
                for n in 1..=32u32 {
                    let mut r = BackwardBits::seeded(&data, bit_pos, total_bits);
                    let expected = reference_peek(&data, bit_pos, n);
                    assert_eq!(
                        r.peek(n),
                        expected,
                        "peek len={len} bit_pos={bit_pos} n={n}"
                    );
                    assert_eq!(
                        r.read(n),
                        expected,
                        "read len={len} bit_pos={bit_pos} n={n}"
                    );
                    assert_eq!(r.bit_pos, bit_pos + n as usize, "read must advance by n");
                }
            }
        }
    }

    /// Many small reads must compose into the same bit string as one large
    /// read — the property the sequence decoder depends on, since it takes
    /// offset, match length and literal length as three consecutive reads.
    #[test]
    fn backward_consecutive_reads_compose() {
        let data = pseudorandom(64, 0xC0FFEE);
        let total_bits = data.len() * 8;
        for split in 1..32u32 {
            let mut one = BackwardBits::seeded(&data, 3, total_bits);
            let mut two = BackwardBits::seeded(&data, 3, total_bits);
            let whole = one.read(32);
            let hi = two.read(split);
            let lo = two.read(32 - split);
            assert_eq!(
                whole,
                (hi << (32 - split)) | lo,
                "split at {split} changed the bit string"
            );
        }
    }

    // ------------------------------------------------------------- forward

    #[test]
    fn forward_reads_lsb_first() {
        // 0b1010_0101 -> reading one bit at a time from the LSB.
        let data = [0b1010_0101u8];
        let mut r = ForwardBits::new(&data);
        for expected in [1, 0, 1, 0, 0, 1, 0, 1] {
            assert_eq!(r.read(1), expected);
        }
    }

    #[test]
    fn forward_multi_bit_reads_assemble_lsb_first() {
        let data = [0b1010_0101u8];
        let mut r = ForwardBits::new(&data);
        assert_eq!(r.read(4), 0b0101);
        assert_eq!(r.read(4), 0b1010);
    }

    /// The FSE header's first field: accuracy log in the low 4 bits.
    #[test]
    fn forward_matches_fse_accuracy_log_convention() {
        let data = [0x2Au8, 0x00];
        let mut r = ForwardBits::new(&data);
        assert_eq!(r.read(4), 0xA, "low nibble first");
    }

    #[test]
    fn forward_crosses_byte_boundaries() {
        let data = [0xFFu8, 0x00];
        let mut r = ForwardBits::new(&data);
        assert_eq!(r.read(4), 0xF);
        assert_eq!(r.read(8), 0x0F, "4 high bits of byte 0, 4 low of byte 1");
    }

    #[test]
    fn forward_peek_does_not_consume() {
        let data = [0b1010_0101u8];
        let mut r = ForwardBits::new(&data);
        assert_eq!(r.peek(4), 0b0101);
        assert_eq!(r.peek(4), 0b0101);
        assert_eq!(r.read(4), 0b0101);
        assert_eq!(r.peek(4), 0b1010);
    }

    #[test]
    fn forward_detects_overrun() {
        let data = [0xFFu8];
        let mut r = ForwardBits::new(&data);
        r.read(8);
        assert!(r.check().is_ok(), "consuming exactly all bits is fine");
        r.read(1);
        assert!(r.check().is_err());
    }

    // ------------------------------------------------------------ backward

    /// The marker convention: `0x80` means one padding bit and no data in that
    /// byte; `0x01` means the whole byte is padding.
    #[test]
    fn backward_marker_positions() {
        let r = BackwardBits::new(&[0xAA, 0x80]).unwrap();
        assert_eq!(r.bits_remaining(), 16 - 1, "0x80: marker is the top bit");

        let r = BackwardBits::new(&[0xAA, 0x01]).unwrap();
        assert_eq!(r.bits_remaining(), 16 - 8, "0x01: byte is entirely padding");

        let r = BackwardBits::new(&[0xAA, 0x10]).unwrap();
        assert_eq!(r.bits_remaining(), 16 - 4);
    }

    #[test]
    fn backward_reads_msb_first_from_the_end() {
        // Last byte 0x01 is pure padding, so reading starts at byte 0's MSB.
        let data = [0b1100_1010u8, 0x01];
        let mut r = BackwardBits::new(&data).unwrap();
        for expected in [1, 1, 0, 0, 1, 0, 1, 0] {
            assert_eq!(r.read(1), expected);
        }
        assert!(r.is_finished());
    }

    #[test]
    fn backward_multi_bit_reads_assemble_msb_first() {
        let data = [0b1100_1010u8, 0x01];
        let mut r = BackwardBits::new(&data).unwrap();
        assert_eq!(r.read(4), 0b1100);
        assert_eq!(r.read(4), 0b1010);
    }

    #[test]
    fn backward_crosses_byte_boundaries_toward_the_front() {
        // Two data bytes plus a pure-padding terminator. Reading proceeds
        // byte[1] then byte[0].
        let data = [0x0Fu8, 0xF0, 0x01];
        let mut r = BackwardBits::new(&data).unwrap();
        assert_eq!(r.read(8), 0xF0, "last data byte comes first");
        assert_eq!(r.read(8), 0x0F);
        assert!(r.is_finished());
    }

    #[test]
    fn backward_reading_zero_bits_is_a_no_op() {
        let data = [0xFFu8, 0x01];
        let mut r = BackwardBits::new(&data).unwrap();
        let before = r.bits_remaining();
        assert_eq!(r.read(0), 0);
        assert_eq!(r.bits_remaining(), before);
    }

    #[test]
    fn backward_rejects_empty_and_zero_terminated_streams() {
        assert!(BackwardBits::new(&[]).is_err());
        assert!(
            BackwardBits::new(&[0xFF, 0x00]).is_err(),
            "a zero final byte has no marker bit"
        );
    }

    #[test]
    fn backward_detects_overrun() {
        let data = [0xAAu8, 0x01];
        let mut r = BackwardBits::new(&data).unwrap();
        r.read(8);
        assert!(r.check().is_ok());
        r.read(4);
        assert!(r.check().is_err());
    }

    /// Round-trip property: bits written MSB-first into a backward stream come
    /// back in the order they were written. Encodes the convention
    /// independently of the reader's implementation.
    #[test]
    fn backward_round_trip_property() {
        // Build a stream carrying the values 0..16 in 5-bit fields.
        let values: Vec<u32> = (0..16).collect();
        let width = 5u32;

        // Assemble the conceptual bit string, then lay it into bytes from the
        // end, adding the marker.
        let mut bit_string: Vec<u8> = Vec::new();
        for &v in &values {
            for i in (0..width).rev() {
                bit_string.push(((v >> i) & 1) as u8);
            }
        }

        let total = bit_string.len();
        let bytes_needed = (total + 1).div_ceil(8);
        let padding = bytes_needed * 8 - total - 1;
        let mut data = alloc::vec![0u8; bytes_needed];

        // The marker sits immediately above the data, and the reversed layout
        // means bit index i lands in byte (len-1 - i/8) at position 7 - i%8.
        let place = |data: &mut Vec<u8>, index: usize, bit: u8| {
            let byte = data.len() - 1 - index / 8;
            data[byte] |= bit << (7 - index % 8);
        };
        place(&mut data, padding, 1);
        for (i, &bit) in bit_string.iter().enumerate() {
            place(&mut data, padding + 1 + i, bit);
        }

        let mut r = BackwardBits::new(&data).unwrap();
        for &expected in &values {
            assert_eq!(r.read(width), expected);
        }
        assert!(r.is_finished(), "stream should end exactly");
    }
}
