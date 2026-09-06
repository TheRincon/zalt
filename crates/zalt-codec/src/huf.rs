//! Huffman decoding for literals (RFC 8878 §4.2).
//!
//! # The 4-stream layout is the performance story
//!
//! A literals section may hold one stream or four. Four is the common case, and
//! it exists precisely so a decoder can run four **independent** dependency
//! chains at once. On an 8-wide core with a deep reorder buffer that is worth
//! more than any amount of vectorisation, because the bit-accumulator chain
//! within a single stream cannot be vectorised at all.
//!
//! [`Table::decode_four`] runs all four in lockstep for this reason. The
//! sequential version it replaced is still the semantics being implemented —
//! stream `i` decodes segment `i`, independently — so the two are
//! interchangeable for correctness and differ only in how much of the machine
//! they keep busy. See `doc/ZALT-MAC.md` §5, ranked win #1.

use alloc::vec::Vec;

use crate::bits::BackwardBits;
use crate::error::{Error, Result};
use crate::fse::{self, DecodeTable, Distribution, State};

/// Largest `tableLog` a literals Huffman table may use.
pub const TABLELOG_MAX: u32 = 11;
/// Largest weight value that can appear in a tree description.
pub const WEIGHT_MAX: u32 = TABLELOG_MAX + 1;
/// Number of streams in the four-stream layout.
pub const STREAMS: usize = 4;
/// Size of the jump table preceding four streams.
pub const JUMP_TABLE_SIZE: usize = 6;

/// One Huffman decoding-table entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Entry {
    symbol: u8,
    nb_bits: u8,
}

/// Two symbols decoded from one table lookup.
///
/// RE-TEST (§5 item #5, §10 sixth pass). For a `table_log`-bit window `w`, the
/// first symbol is the single-symbol entry; the second is real exactly when
/// `a.nb_bits + b.nb_bits <= table_log`, because only then are the bits it
/// consumes all present in the window rather than implied zeroes. That
/// condition is both the pairing rule and its validity test.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Pair {
    sym0: u8,
    sym1: u8,
    /// 1 or 2 — how many of the symbols above are real.
    count: u8,
    /// Bits consumed by the symbols this pair actually emits.
    nb_bits: u8,
}

/// A Huffman decoding table for literals.
#[derive(Debug, Clone, Default)]
pub struct Table {
    entries: Vec<Entry>,
    pairs: Vec<Pair>,
    table_log: u32,
}

impl Table {
    /// Parse a Huffman tree description and build the decoding table.
    ///
    /// Returns the table and the number of bytes the description occupied.
    pub fn parse(input: &[u8]) -> Result<(Self, usize)> {
        let mut table = Self::default();
        let consumed = table.parse_into(input)?;
        Ok((table, consumed))
    }

    /// [`Table::parse`] in place, reusing this table's allocation.
    ///
    /// The decoder holds one Huffman table for the frame in progress, and used
    /// to drop it at every frame boundary — so each frame allocated and zeroed
    /// a fresh `Vec` of up to 2048 entries. On the 3–12 KB documents this
    /// decoder targets that is one `malloc` and one `memset` per document,
    /// against a few kilobytes of output, and `_platform_memset` was 5% of the
    /// profile with `malloc`/`free` another 2.5%.
    pub fn parse_into(&mut self, input: &[u8]) -> Result<usize> {
        let header = *input.first().ok_or(Error::Truncated)?;

        let (weights, consumed) = if header >= 128 {
            // Direct representation: weights are 4-bit, two per byte, high
            // nibble first.
            let count = usize::from(header) - 127;
            let bytes = count.div_ceil(2);
            let body = input.get(1..1 + bytes).ok_or(Error::Truncated)?;
            let mut weights = Vec::with_capacity(count);
            for i in 0..count {
                let byte = body[i / 2];
                weights.push(if i % 2 == 0 { byte >> 4 } else { byte & 0x0F });
            }
            (weights, 1 + bytes)
        } else {
            // FSE-compressed weights; the header byte is the compressed size.
            let size = usize::from(header);
            let body = input.get(1..1 + size).ok_or(Error::Truncated)?;
            (decode_fse_weights(body)?, 1 + size)
        };

        self.build_into(&weights)?;
        Ok(consumed)
    }

    /// Build a table from explicit weights, deriving the final symbol's weight.
    ///
    /// The description omits the last symbol: its weight is whatever makes the
    /// total a power of two. That is not a shortcut — it is how the format
    /// guarantees the tree is complete, and checking it is a real validation
    /// step rather than a formality.
    pub fn from_weights(weights: &[u8]) -> Result<Self> {
        let mut table = Self::default();
        table.build_into(weights)?;
        Ok(table)
    }

    /// [`Table::from_weights`] in place. See [`Table::parse_into`].
    ///
    /// `table_log` is zeroed on entry so a table left half-built by an error
    /// cannot be mistaken for a usable one: a `Treeless` block arriving later
    /// must find no table rather than a plausible one.
    pub fn build_into(&mut self, weights: &[u8]) -> Result<()> {
        self.table_log = 0;
        if weights.is_empty() || weights.len() > 255 {
            return Err(Error::Malformed);
        }

        let mut rank_count = [0u32; (WEIGHT_MAX + 1) as usize];
        let mut weight_total = 0u32;
        for &w in weights {
            if u32::from(w) > WEIGHT_MAX {
                return Err(Error::Malformed);
            }
            rank_count[usize::from(w)] += 1;
            // 2^(w-1) for w > 0, and nothing for w == 0.
            weight_total += (1u32 << w) >> 1;
        }
        if weight_total == 0 {
            return Err(Error::Malformed);
        }

        let table_log = highbit(weight_total) + 1;
        if table_log > TABLELOG_MAX {
            return Err(Error::Malformed);
        }

        // Derive the omitted final weight.
        let total = 1u32 << table_log;
        let rest = total - weight_total;
        if rest == 0 || !rest.is_power_of_two() {
            // The remaining mass must be exactly one power of two, or the tree
            // is not a complete prefix code.
            return Err(Error::Malformed);
        }
        let last_weight = highbit(rest) + 1;
        if last_weight > WEIGHT_MAX {
            return Err(Error::Malformed);
        }

        let mut all = Vec::with_capacity(weights.len() + 1);
        all.extend_from_slice(weights);
        all.push(last_weight as u8);
        rank_count[last_weight as usize] += 1;

        // Some symbol must sit at the bottom of the tree.
        //
        // `nb_bits == table_log + 1 - weight`, so a symbol of weight 1 is
        // exactly the one whose code is `table_log` bits long. A description
        // where no symbol has weight 1 declares a depth nothing reaches: the
        // table is twice the size the code needs, every lookup reads a bit it
        // never consumes, and the same code could have been written with a
        // smaller `table_log`.
        //
        // Such a table still *decodes* — the spans sum to the table size and
        // every state is populated, so nothing downstream objects, which is
        // why this was missed. The reference refuses it, and a differential
        // fuzz campaign found a frame where zalt did not: weights `[0,2,0,2]`
        // at `table_log` 2, giving two symbols of one bit each in a four-entry
        // table. Written minimally the same code is `[1,1]` at `table_log` 1,
        // which both decoders accept.
        if rank_count[1] == 0 {
            return Err(Error::Malformed);
        }

        // Symbols of the same weight occupy a contiguous run; ranks are laid
        // out shortest-code-first.
        let mut rank_start = [0u32; (WEIGHT_MAX + 2) as usize];
        let mut next = 0u32;
        for w in 1..=table_log {
            rank_start[w as usize] = next;
            next += rank_count[w as usize] << (w - 1);
        }

        let table_size = 1usize << table_log;
        if next as usize != table_size {
            return Err(Error::Malformed);
        }

        // Grow to fit, never shrink. A later frame with the same or a smaller
        // `table_log` then reuses the allocation and re-zeroes nothing — the
        // fill below writes every slot in `0..table_size` regardless, because a
        // complete prefix code covers the table exactly.
        if self.entries.len() < table_size {
            self.entries.resize(table_size, Entry::default());
        }
        let entries = &mut self.entries;
        for (symbol, &w) in all.iter().enumerate() {
            if w == 0 {
                continue;
            }
            let w = u32::from(w);
            let span = 1usize << (w - 1);
            let nb_bits = table_log + 1 - w;
            let start = rank_start[w as usize] as usize;
            let slots = entries
                .get_mut(start..start + span)
                .ok_or(Error::Malformed)?;
            for slot in slots {
                *slot = Entry {
                    symbol: symbol as u8,
                    nb_bits: nb_bits as u8,
                };
            }
            rank_start[w as usize] += span as u32;
        }

        // Derive the paired table. This is the cost §10 predicted would sink
        // the idea: `1 << table_log` iterations per table build, which for a
        // small document lands on the workload least able to afford per-frame
        // work.
        if self.pairs.len() < table_size {
            self.pairs.resize(table_size, Pair::default());
        }
        let mask = table_size - 1;
        for w in 0..table_size {
            let a = self.entries[w];
            let mut pair = Pair {
                sym0: a.symbol,
                sym1: 0,
                count: 1,
                nb_bits: a.nb_bits,
            };
            if u32::from(a.nb_bits) < table_log {
                // The window after consuming `a`, refilled with zeroes. Those
                // zeroes are only harmless if `b`'s code fits in the bits that
                // are genuinely present, which is the test below.
                let b = self.entries[(w << a.nb_bits) & mask];
                if u32::from(a.nb_bits) + u32::from(b.nb_bits) <= table_log {
                    pair.sym1 = b.symbol;
                    pair.count = 2;
                    pair.nb_bits = a.nb_bits + b.nb_bits;
                }
            }
            self.pairs[w] = pair;
        }

        self.table_log = table_log;
        Ok(())
    }

    /// Number of states this table actually uses.
    ///
    /// Not `entries.len()`: the allocation is grown to fit and never shrunk, so
    /// it may be larger than the current table. Everything that indexes the
    /// table must bound itself by this.
    #[inline]
    pub fn size(&self) -> usize {
        1usize << self.table_log
    }

    /// Whether a table has been built. `table_log` is at least 1 for any real
    /// table, so zero is unambiguous.
    #[inline]
    pub fn is_built(&self) -> bool {
        self.table_log != 0
    }

    /// Mark unbuilt, keeping the allocation.
    #[inline]
    pub fn invalidate(&mut self) {
        self.table_log = 0;
    }

    /// The table's `tableLog` — the width of a decoding window.
    pub fn table_log(&self) -> u32 {
        self.table_log
    }

    /// Decode `count` symbols from one backward bitstream.
    fn decode_stream(&self, data: &[u8], count: usize, out: &mut Vec<u8>) -> Result<()> {
        if count == 0 {
            return Ok(());
        }
        let mut bits = BackwardBits::new(data)?;
        for _ in 0..count {
            let index = bits.peek(self.table_log) as usize;
            let entry = *self.entries.get(index).ok_or(Error::Malformed)?;
            bits.skip(u32::from(entry.nb_bits));
            out.push(entry.symbol);
            bits.check()?;
        }
        Self::require_consumed(&bits)
    }

    /// A literals stream must end exactly where it says it does.
    ///
    /// The same obligation the sequences bitstream carries, and for the same
    /// reason: the reader returns zero bits past the end rather than failing,
    /// so a stream carrying *more* bits than its symbols consume still decodes
    /// the declared count and reports success. The reference refuses this —
    /// `BIT_endOfDStream` at the end of `HUF_decompress1X1` — and a
    /// differential fuzz campaign found a 19-byte frame where zalt did not:
    /// seven literals of at most two bits each against twenty readable bits.
    ///
    /// `check` alone is not enough. It catches reading *past* the end; this is
    /// the opposite failure, and the two together pin the stream to exactly its
    /// declared length.
    fn require_consumed(bits: &BackwardBits<'_>) -> Result<()> {
        bits.check()?;
        if !bits.is_finished() {
            return Err(Error::Malformed);
        }
        Ok(())
    }

    /// Decode a single-stream literals payload.
    pub fn decode_single(&self, data: &[u8], regenerated: usize, out: &mut Vec<u8>) -> Result<()> {
        self.decode_stream(data, regenerated, out)
    }

    /// Decode a four-stream literals payload.
    ///
    /// The payload opens with a 6-byte jump table holding the sizes of the
    /// first three streams; the fourth's size is whatever remains. Output is
    /// split into four segments, the first three of equal size.
    pub fn decode_four(&self, data: &[u8], regenerated: usize, out: &mut Vec<u8>) -> Result<()> {
        let jump = data.get(..JUMP_TABLE_SIZE).ok_or(Error::Truncated)?;
        let s1 = usize::from(u16::from_le_bytes([jump[0], jump[1]]));
        let s2 = usize::from(u16::from_le_bytes([jump[2], jump[3]]));
        let s3 = usize::from(u16::from_le_bytes([jump[4], jump[5]]));

        let body = &data[JUMP_TABLE_SIZE..];
        // The fourth stream's size is implied. A jump table whose first three
        // sizes exceed the payload would underflow it, so this is checked
        // rather than assumed.
        let used = s1
            .checked_add(s2)
            .and_then(|v| v.checked_add(s3))
            .ok_or(Error::Malformed)?;
        let s4 = body.len().checked_sub(used).ok_or(Error::Malformed)?;

        let segment = regenerated.div_ceil(STREAMS);
        // Also the guard that makes the output split below sound: it fails
        // unless `regenerated >= segment * 3`.
        let last = regenerated
            .checked_sub(segment * (STREAMS - 1))
            .ok_or(Error::Malformed)?;

        let mut offset = 0usize;
        let mut streams = [&[][..]; STREAMS];
        for (slot, size) in streams.iter_mut().zip([s1, s2, s3, s4]) {
            *slot = body.get(offset..offset + size).ok_or(Error::Truncated)?;
            offset += size;
        }

        if regenerated == 0 {
            return Ok(());
        }

        // The output is written by index rather than pushed. `push` costs a
        // capacity check and a length update per symbol; one `resize` pays a
        // single bulk memset and then every write is a store.
        let base = out.len();
        out.resize(base + regenerated, 0);
        let dst = &mut out[base..];
        let (d0, rest) = dst.split_at_mut(segment);
        let (d1, rest) = rest.split_at_mut(segment);
        let (d2, d3) = rest.split_at_mut(segment);

        let mut b0 = BackwardBits::new(streams[0])?;
        let mut b1 = BackwardBits::new(streams[1])?;
        let mut b2 = BackwardBits::new(streams[2])?;
        let mut b3 = BackwardBits::new(streams[3])?;

        // Bound by the *logical* table, not the allocation: `entries` is grown
        // to fit and never shrunk, so a frame with a smaller `table_log` leaves
        // a longer Vec behind. Slicing to `size` restores the property the mask
        // argument below depends on — `i & (size - 1) < size == entries.len()`
        // — so the bounds check folds away exactly as it did before.
        if !self.is_built() {
            return Err(Error::Malformed);
        }
        let log = self.table_log;
        let size = self.size();
        let entries = self.entries.get(..size).ok_or(Error::Malformed)?;
        let mask = size - 1;

        let pairs = self.pairs.get(..size).ok_or(Error::Malformed)?;

        macro_rules! step {
            ($bits:expr, $dst:expr, $i:expr) => {{
                let entry = entries[$bits.peek(log) as usize & mask];
                $dst[$i] = entry.symbol;
                $bits.skip(u32::from(entry.nb_bits));
            }};
        }

        // Two symbols per lookup. Only run where two will fit: a pair whose
        // second symbol has nowhere to go cannot be split, because the entry
        // records the *combined* bit count and skipping it would consume bits
        // belonging to a symbol never written.
        macro_rules! step2 {
            ($bits:expr, $dst:expr, $c:expr) => {{
                let pair = pairs[$bits.peek(log) as usize & mask];
                $dst[$c] = pair.sym0;
                $dst[$c + 1] = pair.sym1;
                $bits.skip(u32::from(pair.nb_bits));
                $c += usize::from(pair.count);
            }};
        }

        // All four streams in lockstep. This is the point of the whole
        // exercise: one symbol's decode depends on the previous symbol's bit
        // count, so a single stream is a serial dependency chain that leaves
        // most of a wide out-of-order core idle. The format splits literals
        // into four independent streams precisely so a decoder can run four
        // such chains at once, and Firestorm is ~8-wide with a deep reorder
        // buffer — it rewards exposing independent work far more than it
        // rewards vectorising a serial loop. No SIMD is involved.
        // Four independent cursors, because streams emitting one or two
        // symbols per step no longer advance together. §10 predicted this is
        // what pairing costs: the shared index that made the interleave free
        // becomes four cursors and four bounds tests.
        let (mut c0, mut c1, mut c2, mut c3) = (0usize, 0usize, 0usize, 0usize);

        while c0 + 2 <= segment && c1 + 2 <= segment && c2 + 2 <= segment && c3 + 2 <= last {
            step2!(b0, d0, c0);
            step2!(b1, d1, c1);
            step2!(b2, d2, c2);
            step2!(b3, d3, c3);
        }

        // Each stream finishes alone: pairs while two still fit, then the
        // single-symbol table for the last byte.
        macro_rules! finish {
            ($bits:expr, $dst:expr, $c:expr, $limit:expr) => {{
                while $c + 2 <= $limit {
                    step2!($bits, $dst, $c);
                }
                while $c < $limit {
                    step!($bits, $dst, $c);
                    $c += 1;
                }
            }};
        }
        finish!(b0, d0, c0, segment);
        finish!(b1, d1, c1, segment);
        finish!(b2, d2, c2, segment);
        finish!(b3, d3, c3, last);

        // Checked once per stream rather than once per symbol: reading past the
        // end yields zero bits rather than failing, so an overrunning stream
        // decodes to junk and is caught here — the same rejection, one branch
        // per stream instead of one per literal.
        //
        // Every one of the four must also be *exactly* consumed; see
        // [`Table::require_consumed`].
        Self::require_consumed(&b0)?;
        Self::require_consumed(&b1)?;
        Self::require_consumed(&b2)?;
        Self::require_consumed(&b3)?;
        Ok(())
    }
}

/// Decode FSE-compressed Huffman weights.
///
/// The generic FSE decompressor runs **two interleaved states**, alternating
/// between them, and stops when the bitstream is spent. The final symbol comes
/// from the state that did not just consume the last bits.
fn decode_fse_weights(data: &[u8]) -> Result<Vec<u8>> {
    let (distribution, header_len) = Distribution::parse(data, fse::HUF_WEIGHT_ACCURACY_MAX, 255)?;
    let table = DecodeTable::build(&distribution)?;
    let payload = data.get(header_len..).ok_or(Error::Truncated)?;

    let mut bits = BackwardBits::new(payload)?;
    let mut s1 = State::new(&table, &mut bits)?;
    let mut s2 = State::new(&table, &mut bits)?;

    let mut weights = Vec::new();
    // 255 symbols max, and the description omits the last one.
    let limit = 255usize;

    // Emitting a symbol *includes* advancing its state, and the end-of-stream
    // test comes **after** that advance. Testing before it — the intuitive
    // reading — stops one symbol early, which shifts every symbol index past
    // the truncation point down by one. That failure is nearly invisible: the
    // table still builds and almost all output is correct, because only the
    // very last symbol is misplaced.
    loop {
        if weights.len() >= limit {
            return Err(Error::Malformed);
        }
        weights.push(s1.symbol(&table)?);
        s1.advance(&table, &mut bits)?;
        if bits.exhausted() {
            weights.push(s2.symbol(&table)?);
            break;
        }

        if weights.len() >= limit {
            return Err(Error::Malformed);
        }
        weights.push(s2.symbol(&table)?);
        s2.advance(&table, &mut bits)?;
        if bits.exhausted() {
            weights.push(s1.symbol(&table)?);
            break;
        }
    }

    Ok(weights)
}

#[inline]
fn highbit(value: u32) -> u32 {
    debug_assert!(value > 0);
    31 - value.leading_zeros()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A description declaring a depth no symbol reaches must be refused.
    ///
    /// `[0, 2, 0]` derives a final weight of 2, giving two symbols of one bit
    /// each in a four-entry table — `table_log` 2 for a code that is one bit
    /// deep. It decodes perfectly well, which is why nothing downstream
    /// objected; the reference refuses it, and a differential fuzz campaign
    /// found a frame built on it.
    #[test]
    fn a_table_deeper_than_its_longest_code_is_refused() {
        assert!(
            Table::from_weights(&[0, 2, 0]).is_err(),
            "table_log 2 with no symbol at depth 2 must not build"
        );

        // Written minimally the same code is legal, and must stay so.
        let canonical = Table::from_weights(&[1]).expect("two symbols, one bit each");
        assert_eq!(canonical.table_log(), 1);
        assert_eq!(canonical.entries.len(), 2);
    }

    /// Four symbols with equal weights form a complete 2-bit code.
    #[test]
    fn uniform_weights_build_a_flat_table() {
        // Three explicit weights; the fourth is derived.
        let table = Table::from_weights(&[1, 1, 1]).unwrap();
        assert_eq!(table.table_log(), 2);
        assert_eq!(table.entries.len(), 4);
        for e in &table.entries {
            assert_eq!(e.nb_bits, 2, "a flat 4-symbol code is 2 bits everywhere");
        }
        let symbols: Vec<u8> = table.entries.iter().map(|e| e.symbol).collect();
        assert_eq!(symbols, alloc::vec![0, 1, 2, 3]);
    }

    /// Weights encode code *lengths* inversely: a larger weight is a shorter
    /// code, and doubles the number of table slots.
    #[test]
    fn larger_weight_means_shorter_code_and_more_slots() {
        // weights [2, 1] -> derived last weight 1. Total = 2 + 1 + 1 = 4.
        let table = Table::from_weights(&[2, 1]).unwrap();
        assert_eq!(table.table_log(), 2);

        let mut slots = [0usize; 3];
        for e in &table.entries {
            slots[e.symbol as usize] += 1;
        }
        assert_eq!(slots[0], 2, "weight 2 occupies two slots");
        assert_eq!(slots[1], 1);
        assert_eq!(slots[2], 1);

        // Entries are indexed by *code*, not by symbol, so look the symbol up.
        // zstd lays the longest codes (weight 1) at the low indices and the
        // shortest at the high ones; asserting on `entries[0]` would be
        // asserting on whichever symbol happens to sort first.
        let symbol0 = table.entries.iter().find(|e| e.symbol == 0).unwrap();
        assert_eq!(symbol0.nb_bits, 1, "the frequent symbol decodes in one bit");
        let symbol1 = table.entries.iter().find(|e| e.symbol == 1).unwrap();
        assert_eq!(symbol1.nb_bits, 2);
    }

    #[test]
    fn derived_last_weight_completes_the_tree() {
        // weights [1,1,1] -> total 3, rest 1 -> last weight 1, four symbols.
        let table = Table::from_weights(&[1, 1, 1]).unwrap();
        let distinct: alloc::collections::BTreeSet<u8> =
            table.entries.iter().map(|e| e.symbol).collect();
        assert_eq!(distinct.len(), 4, "the omitted symbol must appear");
    }

    #[test]
    fn incomplete_tree_is_rejected() {
        // Five weights of 1 sum to 5. The table is 8, leaving 3 — not a power
        // of two, so no single derived weight can complete the code.
        assert!(Table::from_weights(&[1, 1, 1, 1, 1]).is_err());
        // Likewise 6 of them: 8 - 6 = 2 is fine, but 7 leaves 1... check a
        // clearly broken mix instead.
        assert!(Table::from_weights(&[3, 1, 1, 1, 1, 1]).is_err());
    }

    /// Four weights of 1 *is* valid: the table is 8, leaving 4, so the derived
    /// symbol takes weight 3. Recorded because it looks like the invalid case
    /// above and is not.
    #[test]
    fn four_equal_weights_are_completed_by_a_short_code() {
        let table = Table::from_weights(&[1, 1, 1, 1]).unwrap();
        assert_eq!(table.table_log(), 3);
        let derived = table.entries.iter().find(|e| e.symbol == 4).unwrap();
        assert_eq!(
            derived.nb_bits, 1,
            "the derived symbol takes the short code"
        );
    }

    #[test]
    fn rejects_weights_that_overflow_the_table_log() {
        assert!(Table::from_weights(&[12, 12, 12]).is_err());
    }

    #[test]
    fn rejects_empty_and_all_zero_weights() {
        assert!(Table::from_weights(&[]).is_err());
        assert!(Table::from_weights(&[0, 0, 0]).is_err());
    }

    /// The direct (non-FSE) tree description: header >= 128, 4-bit weights,
    /// high nibble first.
    #[test]
    fn parses_direct_weight_description() {
        // header 131 -> 131 - 127 = 4 weights, packed two per byte.
        let input = [131u8, 0x11, 0x11];
        let (table, consumed) = Table::parse(&input).unwrap();
        assert_eq!(consumed, 1 + 2);
        assert_eq!(table.table_log(), 3);
        // Four explicit symbols plus the derived one.
        let distinct: alloc::collections::BTreeSet<u8> =
            table.entries.iter().map(|e| e.symbol).collect();
        assert_eq!(distinct.len(), 5);
    }

    #[test]
    fn direct_description_nibble_order() {
        // 3 weights: 2, 1, 1 -> total 4, rest 4-4=0 -> invalid; use 2,1 only.
        let input = [129u8, 0x21];
        let (table, consumed) = Table::parse(&input).unwrap();
        assert_eq!(consumed, 2);
        assert_eq!(table.table_log(), 2);
    }

    #[test]
    fn truncated_description_is_reported() {
        assert!(Table::parse(&[]).is_err());
        assert!(
            Table::parse(&[200]).is_err(),
            "claims weights that are absent"
        );
        assert!(
            Table::parse(&[10]).is_err(),
            "claims 10 FSE bytes that are absent"
        );
    }

    /// Round-trip: encode symbols with a known flat code and decode them back.
    #[test]
    fn decodes_a_flat_code_round_trip() {
        let table = Table::from_weights(&[1, 1, 1]).unwrap();
        // Flat 2-bit code: symbol N encodes as the 2-bit value N.
        let symbols = [0u8, 1, 2, 3, 3, 2, 1, 0];

        // Lay the codes into a backward bitstream, MSB-first, with a marker.
        let mut bit_string = Vec::new();
        for &s in &symbols {
            bit_string.push((s >> 1) & 1);
            bit_string.push(s & 1);
        }
        let total = bit_string.len();
        let bytes_needed = (total + 1).div_ceil(8);
        let padding = bytes_needed * 8 - total - 1;
        let mut data = alloc::vec![0u8; bytes_needed];
        let place = |data: &mut Vec<u8>, index: usize, bit: u8| {
            let byte = data.len() - 1 - index / 8;
            data[byte] |= bit << (7 - index % 8);
        };
        place(&mut data, padding, 1);
        for (i, &bit) in bit_string.iter().enumerate() {
            place(&mut data, padding + 1 + i, bit);
        }

        let mut out = Vec::new();
        table.decode_single(&data, symbols.len(), &mut out).unwrap();
        assert_eq!(out, symbols);
    }

    #[test]
    fn four_stream_rejects_a_jump_table_that_overruns() {
        let table = Table::from_weights(&[1, 1, 1]).unwrap();
        // Jump table claims three 1000-byte streams inside a 10-byte payload.
        let mut data = alloc::vec![0u8; JUMP_TABLE_SIZE + 4];
        data[0..2].copy_from_slice(&1000u16.to_le_bytes());
        data[2..4].copy_from_slice(&1000u16.to_le_bytes());
        data[4..6].copy_from_slice(&1000u16.to_le_bytes());
        let mut out = Vec::new();
        assert!(table.decode_four(&data, 16, &mut out).is_err());
    }
}
