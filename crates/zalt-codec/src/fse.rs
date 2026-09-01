//! Finite State Entropy: table description parsing and decoding (RFC 8878 §4.1).
//!
//! FSE appears twice in zstd, and it is worth being clear which is which:
//!
//! 1. Compressing the **Huffman weight** table in a literals section.
//! 2. Coding the **literals-length, match-length and offset** symbols in a
//!    sequences section.
//!
//! Both use the same table machinery; they differ in accuracy-log limits,
//! symbol counts, and whether predefined distributions exist.
//!
//! # Why decoding is inherently serial
//!
//! Each state transition depends on the previous state, so a single FSE stream
//! cannot be vectorised — the dependency chain defeats it. The parallelism zstd
//! offers is *across* streams: a sequences section runs three independent states
//! (LL, ML, OF) that can be interleaved to fill a wide core. That is item #4 in
//! `doc/ZALT-MAC.md` §5, and it is why this module exposes state advance as a
//! separate step rather than hiding it inside symbol decoding.

use crate::bits::{BackwardBits, ForwardBits};
use crate::error::{Error, Result};

/// Largest accuracy log any zstd FSE table may declare.
pub const ACCURACY_LOG_MAX: u32 = 9;

/// Largest decoding table, in entries.
pub const TABLE_SIZE_MAX: usize = 1 << ACCURACY_LOG_MAX;

/// Accuracy-log ceiling for literals-length tables.
pub const LL_ACCURACY_MAX: u32 = 9;
/// Accuracy-log ceiling for match-length tables.
pub const ML_ACCURACY_MAX: u32 = 9;
/// Accuracy-log ceiling for offset-code tables.
pub const OF_ACCURACY_MAX: u32 = 8;
/// Accuracy-log ceiling for Huffman weight tables.
pub const HUF_WEIGHT_ACCURACY_MAX: u32 = 6;

/// Highest literals-length symbol.
pub const LL_SYMBOL_MAX: u16 = 35;
/// Highest match-length symbol.
pub const ML_SYMBOL_MAX: u16 = 52;
/// Highest offset-code symbol.
pub const OF_SYMBOL_MAX: u16 = 31;

/// One decoding-table entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Entry {
    /// Symbol this state emits.
    pub symbol: u8,
    /// Bits to read when advancing out of this state.
    pub nb_bits: u8,
    /// Base of the next state, before the bits read are added.
    pub new_state: u16,
}

/// A normalized distribution, as written in an FSE table description.
///
/// Counts are signed: `-1` means "probability less than one", a symbol that
/// still occupies a table slot. Treating it as unsigned is a classic way to
/// build a table that decodes plausible nonsense.
#[derive(Debug, Clone)]
pub struct Distribution {
    counts: [i16; 256],
    max_symbol: u16,
    accuracy_log: u32,
}

impl Distribution {
    /// Build a distribution from explicit counts, for the predefined tables.
    pub fn from_counts(counts: &[i16], accuracy_log: u32) -> Result<Self> {
        if counts.is_empty() || counts.len() > 256 || accuracy_log > ACCURACY_LOG_MAX {
            return Err(Error::Malformed);
        }
        let mut c = [0i16; 256];
        c[..counts.len()].copy_from_slice(counts);
        let d = Self {
            counts: c,
            max_symbol: counts.len() as u16 - 1,
            accuracy_log,
        };
        d.validate()?;
        Ok(d)
    }

    /// The distribution's accuracy log.
    pub fn accuracy_log(&self) -> u32 {
        self.accuracy_log
    }

    /// Highest symbol carrying a count.
    pub fn max_symbol(&self) -> u16 {
        self.max_symbol
    }

    /// Counts must sum, in absolute value, to exactly the table size.
    fn validate(&self) -> Result<()> {
        let table_size = 1i32 << self.accuracy_log;
        let sum: i32 = (0..=self.max_symbol as usize)
            .map(|s| i32::from(self.counts[s]).abs())
            .sum();
        if sum != table_size {
            return Err(Error::Malformed);
        }
        Ok(())
    }

    /// Parse an FSE table description from a forward bitstream (RFC 8878 §4.1.1).
    ///
    /// Returns the distribution and the number of whole bytes consumed — the
    /// format defines the description's length that way, so the caller can find
    /// what follows it.
    pub fn parse(input: &[u8], max_accuracy_log: u32, max_symbol: u16) -> Result<(Self, usize)> {
        let mut bits = ForwardBits::new(input);

        let accuracy_log = bits.read(4) + 5;
        if accuracy_log > max_accuracy_log {
            return Err(Error::Malformed);
        }

        let table_size = 1i32 << accuracy_log;
        let mut remaining = table_size + 1;
        let mut threshold = table_size;
        let mut nb_bits = accuracy_log + 1;

        let mut counts = [0i16; 256];
        let mut symbol: usize = 0;

        while remaining > 1 && symbol <= max_symbol as usize {
            // The count is coded in a variable width that narrows as the
            // remaining probability mass shrinks. `max` is the number of values
            // representable in one fewer bit.
            let max = (2 * threshold - 1) - remaining;
            let value = bits.peek(nb_bits) as i32;

            let count = if (value & (threshold - 1)) < max {
                bits.skip(nb_bits - 1);
                value & (threshold - 1)
            } else {
                bits.skip(nb_bits);
                let mut c = value & (2 * threshold - 1);
                if c >= threshold {
                    c -= max;
                }
                c
            };

            // Counts are stored biased by one so that -1 is expressible.
            let count = count - 1;
            remaining -= count.abs();

            if symbol >= 256 {
                return Err(Error::Malformed);
            }
            counts[symbol] = i16::try_from(count).map_err(|_| Error::Malformed)?;
            symbol += 1;

            // A zero count introduces a run-length escape for further zeros,
            // coded two bits at a time with 3 meaning "continue".
            if count == 0 {
                loop {
                    let repeat = bits.read(2) as usize;
                    let end = symbol + repeat;
                    if end > 256 {
                        return Err(Error::Malformed);
                    }
                    while symbol < end {
                        counts[symbol] = 0;
                        symbol += 1;
                    }
                    if repeat < 3 {
                        break;
                    }
                }
            }

            while remaining < threshold {
                // The arithmetic above keeps `remaining >= 1`, which bounds
                // this loop — but that argument depends on an invariant several
                // statements away, and "I proved it cannot happen" is how
                // decoders acquire hangs. A malformed distribution must cost an
                // error, never an underflow or a spin.
                if nb_bits == 0 {
                    return Err(Error::Malformed);
                }
                nb_bits -= 1;
                threshold >>= 1;
            }

            bits.check()?;
        }

        if remaining != 1 || symbol == 0 {
            return Err(Error::Malformed);
        }
        bits.check()?;

        let distribution = Self {
            counts,
            max_symbol: symbol as u16 - 1,
            accuracy_log,
        };
        distribution.validate()?;

        Ok((distribution, bits.bytes_read()))
    }
}

/// An FSE decoding table.
#[derive(Debug, Clone)]
pub struct DecodeTable {
    entries: [Entry; TABLE_SIZE_MAX],
    accuracy_log: u32,
    size: usize,
}

impl Default for DecodeTable {
    fn default() -> Self {
        Self {
            entries: [Entry::default(); TABLE_SIZE_MAX],
            accuracy_log: 0,
            size: 0,
        }
    }
}

impl DecodeTable {
    /// Build a decoding table from a normalized distribution.
    pub fn build(distribution: &Distribution) -> Result<Self> {
        let accuracy_log = distribution.accuracy_log;
        let table_size = 1usize << accuracy_log;
        let table_mask = table_size - 1;
        // The spread step is chosen to visit every slot exactly once; it is
        // coprime with the table size for every legal accuracy log.
        let step = (table_size >> 1) + (table_size >> 3) + 3;

        let mut symbols = [0u8; TABLE_SIZE_MAX];
        let mut symbol_next = [0u16; 256];
        // Slots above `high_threshold` are reserved for "less than one"
        // symbols, filled downward from the top of the table.
        let mut high_threshold = table_size;

        // Indexing by symbol, not iterating: this loop touches three parallel
        // arrays (`counts`, `symbol_next`, `symbols`) plus a descending
        // cursor, and a zip would obscure that they share one index space.
        #[allow(clippy::needless_range_loop)]
        for s in 0..=distribution.max_symbol as usize {
            let count = distribution.counts[s];
            if count == -1 {
                if high_threshold == 0 {
                    return Err(Error::Malformed);
                }
                high_threshold -= 1;
                symbols[high_threshold] = u8::try_from(s).map_err(|_| Error::Malformed)?;
                symbol_next[s] = 1;
            } else {
                symbol_next[s] = count.max(0) as u16;
            }
        }

        // Spread the remaining symbols, skipping the reserved region.
        let mut position = 0usize;
        for s in 0..=distribution.max_symbol as usize {
            let count = distribution.counts[s];
            if count <= 0 {
                continue;
            }
            for _ in 0..count {
                symbols[position] = u8::try_from(s).map_err(|_| Error::Malformed)?;
                position = (position + step) & table_mask;
                while position >= high_threshold {
                    position = (position + step) & table_mask;
                }
            }
        }
        // A correct spread walks the whole table and returns to the start.
        // Anything else means the distribution did not sum to the table size,
        // which `validate` should already have caught.
        if position != 0 {
            return Err(Error::Malformed);
        }

        let mut entries = [Entry::default(); TABLE_SIZE_MAX];
        for (slot, entry) in entries.iter_mut().enumerate().take(table_size) {
            let symbol = symbols[slot];
            let next = symbol_next[symbol as usize];
            symbol_next[symbol as usize] += 1;
            // Symbols appearing often need fewer bits to distinguish their
            // states; `next` counts occurrences so far.
            let nb_bits = accuracy_log - highbit(u32::from(next));
            let new_state = (u32::from(next) << nb_bits) - table_size as u32;
            *entry = Entry {
                symbol,
                nb_bits: nb_bits as u8,
                new_state: u16::try_from(new_state).map_err(|_| Error::Malformed)?,
            };
        }

        Ok(Self {
            entries,
            accuracy_log,
            size: table_size,
        })
    }

    /// Build directly from explicit counts.
    pub fn from_counts(counts: &[i16], accuracy_log: u32) -> Result<Self> {
        Self::build(&Distribution::from_counts(counts, accuracy_log)?)
    }

    /// A table that always emits the same symbol, for `RLE` compression mode.
    pub fn rle(symbol: u8) -> Self {
        let mut entries = [Entry::default(); TABLE_SIZE_MAX];
        entries[0] = Entry {
            symbol,
            nb_bits: 0,
            new_state: 0,
        };
        Self {
            entries,
            accuracy_log: 0,
            size: 1,
        }
    }

    /// The table's accuracy log — the bit width of an initial state.
    pub fn accuracy_log(&self) -> u32 {
        self.accuracy_log
    }

    /// Number of states.
    pub fn len(&self) -> usize {
        self.size
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// Entry for a state.
    #[inline]
    pub fn entry(&self, state: u16) -> Result<Entry> {
        self.entries
            .get(state as usize)
            .filter(|_| (state as usize) < self.size)
            .copied()
            .ok_or(Error::Malformed)
    }

    /// The entry for `state`, with the index folded into the table.
    ///
    /// # Why the hot path does not use [`DecodeTable::entry`]
    ///
    /// A valid table can only ever produce in-range states: the initial state
    /// is `accuracy_log` bits, and every transition is `new_state + added` with
    /// both terms bounded by the construction that already rejected malformed
    /// distributions. So the range check in `entry` never fires on input that
    /// got this far, and it was costing a branch and a `Result` six times per
    /// sequence.
    ///
    /// `size` is `1 << accuracy_log`, so masking by `size - 1` is the identity
    /// for every state that check would have accepted. It is also
    /// **unconditionally memory-safe** regardless of the table: `entries` is a
    /// fixed `[Entry; TABLE_SIZE_MAX]` array and `size` never exceeds it, so
    /// the masked index is always within the array — this trades an error for
    /// wrong output on input that should already have been rejected, never for
    /// an out-of-bounds access. A frame that reached this state with a bad
    /// table produces garbage that its checksum then refuses, which is how
    /// every other wrong-bits case in the decoder already behaves.
    #[inline]
    pub fn entry_masked(&self, state: u16) -> Entry {
        debug_assert!(self.size.is_power_of_two());
        debug_assert!(self.size <= TABLE_SIZE_MAX);
        self.entries[(state as usize) & (self.size.max(1) - 1)]
    }
}

/// Floor of log2. `highbit(1) == 0`, `highbit(0)` is defined as 0.
#[inline]
fn highbit(value: u32) -> u32 {
    if value == 0 {
        return 0;
    }
    31 - value.leading_zeros()
}

/// A live FSE decoding state over a backward bitstream.
///
/// Symbol reading and state advance are deliberately separate operations. The
/// sequences section requires that order — read all three symbols, then read
/// their value bits, then advance all three states — and a combined
/// `decode_symbol` would make that impossible to express.
#[derive(Debug, Clone, Copy)]
pub struct State {
    value: u16,
}

impl State {
    /// Initialise from the stream by reading `accuracy_log` bits.
    pub fn new(table: &DecodeTable, bits: &mut BackwardBits<'_>) -> Result<Self> {
        let value = bits.read(table.accuracy_log()) as u16;
        bits.check()?;
        Ok(Self { value })
    }

    /// The symbol this state emits, without advancing.
    pub fn symbol(&self, table: &DecodeTable) -> Result<u8> {
        Ok(table.entry(self.value)?.symbol)
    }

    /// Advance to the next state, consuming this state's bits.
    pub fn advance(&mut self, table: &DecodeTable, bits: &mut BackwardBits<'_>) -> Result<()> {
        let entry = table.entry(self.value)?;
        self.advance_with(entry, bits);
        Ok(())
    }

    /// This state's entry, for callers that need it twice.
    ///
    /// The sequences loop reads a symbol and later advances out of the same
    /// state, and both want the same entry. Fetching it once and passing it to
    /// [`State::advance_with`] halves the table lookups in the hottest loop in
    /// the decoder.
    #[inline]
    pub fn entry(&self, table: &DecodeTable) -> Entry {
        table.entry_masked(self.value)
    }

    /// Advance using an entry already fetched by [`State::entry`].
    #[inline]
    pub fn advance_with(&mut self, entry: Entry, bits: &mut BackwardBits<'_>) {
        let added = bits.read(u32::from(entry.nb_bits)) as u16;
        self.advance_by(entry, added);
    }

    /// Advance using an entry and transition bits the caller already read.
    ///
    /// The sequences loop reads all three states' transition bits as one
    /// field and splits it, so it has the bits before it has a state to give
    /// them to.
    #[inline]
    pub fn advance_by(&mut self, entry: Entry, added: u16) {
        self.value = entry.new_state.wrapping_add(added);
    }

    /// Raw state value, for diagnostics.
    pub fn raw(&self) -> u16 {
        self.value
    }
}

// ------------------------------------------------------- predefined tables

/// Predefined literals-length distribution (RFC 8878 §3.1.1.3.2.2.1).
pub const LL_DEFAULT: [i16; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];

/// Predefined match-length distribution.
///
/// Transcribed row-for-row from RFC 8878 §3.1.1.3.2.2.2 (`doc/rfc8878.txt`).
/// Note where the `-1` run begins: **index 46**, not 48. An earlier version had
/// 39 ones and 5 minus-ones instead of 37 and 7, which still sums to 64 — so
/// the table built, every self-consistency check passed, and only the
/// state-to-symbol mapping was wrong. See `doc/ZALT-MAC.md` §10.
pub const ML_DEFAULT: [i16; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, //
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, //
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, //
    -1, -1, -1, -1, -1,
];

/// Predefined offset-code distribution.
pub const OF_DEFAULT: [i16; 29] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];

/// Accuracy log of the predefined literals-length table.
pub const LL_DEFAULT_ACCURACY: u32 = 6;
/// Accuracy log of the predefined match-length table.
pub const ML_DEFAULT_ACCURACY: u32 = 6;
/// Accuracy log of the predefined offset-code table.
pub const OF_DEFAULT_ACCURACY: u32 = 5;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn highbit_is_floor_log2() {
        assert_eq!(highbit(0), 0);
        assert_eq!(highbit(1), 0);
        assert_eq!(highbit(2), 1);
        assert_eq!(highbit(3), 1);
        assert_eq!(highbit(4), 2);
        assert_eq!(highbit(255), 7);
        assert_eq!(highbit(256), 8);
    }

    /// The predefined distributions are transcribed from the RFC. Their counts
    /// must sum in absolute value to the table size — which is exactly the
    /// check that catches a transcription slip.
    #[test]
    fn predefined_distributions_sum_to_their_table_size() {
        for (name, counts, accuracy, expect) in [
            ("LL", &LL_DEFAULT[..], LL_DEFAULT_ACCURACY, 64),
            ("ML", &ML_DEFAULT[..], ML_DEFAULT_ACCURACY, 64),
            ("OF", &OF_DEFAULT[..], OF_DEFAULT_ACCURACY, 32),
        ] {
            let sum: i32 = counts.iter().map(|c| i32::from(*c).abs()).sum();
            assert_eq!(
                sum, expect,
                "{name} distribution sums to {sum}, want {expect}"
            );
            assert_eq!(1i32 << accuracy, expect, "{name} accuracy log mismatch");
        }
    }

    /// **The check the sum test cannot make.**
    ///
    /// `predefined_distributions_sum_to_their_table_size` passed for a version
    /// of `ML_DEFAULT` that had 39 ones and 5 minus-ones instead of 37 and 7 —
    /// because both sum to 64. Every table built, every internal consistency
    /// check held, and the only symptom was that some states decoded to the
    /// wrong symbol. So this pins the shape, not just the total.
    #[test]
    fn predefined_distributions_have_the_exact_shape_from_the_rfc() {
        fn tally(counts: &[i16]) -> (usize, usize, usize, usize) {
            let ones = counts.iter().filter(|c| **c == 1).count();
            let twos = counts.iter().filter(|c| **c == 2).count();
            let low = counts.iter().filter(|c| **c == -1).count();
            (counts.len(), ones, twos, low)
        }

        // RFC 8878 §3.1.1.3.2.2.1
        assert_eq!(tally(&LL_DEFAULT), (36, 8, 21, 4));
        assert_eq!(LL_DEFAULT[0], 4);
        assert_eq!(LL_DEFAULT[25], 3);

        // RFC 8878 §3.1.1.3.2.2.2 — the `-1` run starts at index 46.
        // 38 ones, not 37: index 0 is a lone `1` before the six `2`s, separate
        // from the 37-long run at indices 9..=45.
        assert_eq!(tally(&ML_DEFAULT), (53, 38, 6, 7));
        assert_eq!(ML_DEFAULT[1], 4);
        assert_eq!(ML_DEFAULT[45], 1, "last one before the -1 run");
        assert_eq!(ML_DEFAULT[46], -1, "the -1 run begins here, not at 48");

        // RFC 8878 §3.1.1.3.2.2.3
        assert_eq!(tally(&OF_DEFAULT), (29, 21, 3, 5));
        assert_eq!(OF_DEFAULT[6], 2);
    }

    /// Golden state-to-symbol mappings for the predefined tables.
    ///
    /// These come from decoding a real `zstd` frame that uses `Predefined` mode
    /// for all three symbol types: the frame's only sequence is
    /// `literals_length=2, match_length=87, offset=1`, which forces
    /// `ll_code=2`, `ml_code=41`, `of_code=0` at states 24, 19 and 0.
    ///
    /// A distribution error shifts these, which is precisely how the original
    /// `ML_DEFAULT` bug showed itself.
    #[test]
    fn predefined_tables_map_known_states_to_known_symbols() {
        let ll = DecodeTable::from_counts(&LL_DEFAULT, LL_DEFAULT_ACCURACY).unwrap();
        let ml = DecodeTable::from_counts(&ML_DEFAULT, ML_DEFAULT_ACCURACY).unwrap();
        let of = DecodeTable::from_counts(&OF_DEFAULT, OF_DEFAULT_ACCURACY).unwrap();

        assert_eq!(ll.entry(24).unwrap().symbol, 2, "LL state 24");
        assert_eq!(
            ml.entry(19).unwrap().symbol,
            41,
            "ML state 19 — was 43 with the wrong -1 split"
        );
        assert_eq!(of.entry(0).unwrap().symbol, 0, "OF state 0");
    }

    #[test]
    fn predefined_symbol_counts_match_the_spec() {
        assert_eq!(LL_DEFAULT.len() as u16 - 1, LL_SYMBOL_MAX);
        assert_eq!(ML_DEFAULT.len() as u16 - 1, ML_SYMBOL_MAX);
        assert_eq!(OF_DEFAULT.len() as u16 - 1, OF_SYMBOL_MAX - 3);
    }

    #[test]
    fn predefined_tables_build() {
        for (counts, accuracy, size) in [
            (&LL_DEFAULT[..], LL_DEFAULT_ACCURACY, 64),
            (&ML_DEFAULT[..], ML_DEFAULT_ACCURACY, 64),
            (&OF_DEFAULT[..], OF_DEFAULT_ACCURACY, 32),
        ] {
            let table = DecodeTable::from_counts(counts, accuracy).unwrap();
            assert_eq!(table.len(), size);
            assert_eq!(table.accuracy_log(), accuracy);
        }
    }

    /// Every state must be reachable and every entry well formed: `nb_bits`
    /// within the accuracy log, and `new_state + max_added` still inside the
    /// table. A table failing this decodes into out-of-range states on real
    /// input.
    #[test]
    fn built_tables_are_internally_consistent() {
        for (counts, accuracy) in [
            (&LL_DEFAULT[..], LL_DEFAULT_ACCURACY),
            (&ML_DEFAULT[..], ML_DEFAULT_ACCURACY),
            (&OF_DEFAULT[..], OF_DEFAULT_ACCURACY),
        ] {
            let table = DecodeTable::from_counts(counts, accuracy).unwrap();
            for state in 0..table.len() as u16 {
                let e = table.entry(state).unwrap();
                assert!(
                    u32::from(e.nb_bits) <= accuracy,
                    "state {state}: nb_bits {} exceeds accuracy {accuracy}",
                    e.nb_bits
                );
                let max_next = u32::from(e.new_state) + ((1u32 << e.nb_bits) - 1);
                assert!(
                    (max_next as usize) < table.len(),
                    "state {state}: can transition to {max_next}, table size {}",
                    table.len()
                );
            }
        }
    }

    /// Each symbol must occupy exactly as many slots as its normalized count
    /// (or one slot, for a `-1` symbol). This is the property the spread
    /// algorithm exists to satisfy.
    #[test]
    fn spread_gives_each_symbol_its_share() {
        let table = DecodeTable::from_counts(&LL_DEFAULT, LL_DEFAULT_ACCURACY).unwrap();
        let mut seen = [0i32; 256];
        for state in 0..table.len() as u16 {
            seen[table.entry(state).unwrap().symbol as usize] += 1;
        }
        for (symbol, &count) in LL_DEFAULT.iter().enumerate() {
            let expected = if count == -1 { 1 } else { count as i32 };
            assert_eq!(seen[symbol], expected, "symbol {symbol}");
        }
    }

    #[test]
    fn rle_table_always_emits_its_symbol() {
        let table = DecodeTable::rle(42);
        assert_eq!(table.len(), 1);
        assert_eq!(table.accuracy_log(), 0);
        let e = table.entry(0).unwrap();
        assert_eq!(e.symbol, 42);
        assert_eq!(e.nb_bits, 0);
    }

    #[test]
    fn out_of_range_state_is_an_error_not_a_panic() {
        let table = DecodeTable::from_counts(&OF_DEFAULT, OF_DEFAULT_ACCURACY).unwrap();
        assert!(table.entry(31).is_ok());
        assert!(table.entry(32).is_err(), "past the end of a 32-entry table");
        assert!(table.entry(u16::MAX).is_err());
    }

    /// The claim `entry_masked` is built on, checked rather than asserted.
    ///
    /// The hot loop dropped the range check that `entry` performs, on the
    /// argument that a valid table cannot produce an out-of-range state and
    /// that masking is memory-safe regardless. The second half is the one that
    /// matters for hostile input, and it is the one worth proving: **every**
    /// `u16`, including values no valid table could reach, must land inside the
    /// table rather than past it.
    #[test]
    fn masking_keeps_every_possible_state_inside_the_table() {
        for (counts, log) in [
            (&OF_DEFAULT[..], OF_DEFAULT_ACCURACY),
            (&LL_DEFAULT[..], LL_DEFAULT_ACCURACY),
            (&ML_DEFAULT[..], ML_DEFAULT_ACCURACY),
        ] {
            let table = DecodeTable::from_counts(counts, log).unwrap();
            let size = table.len();
            assert!(size.is_power_of_two(), "masking assumes a power of two");
            assert!(size <= TABLE_SIZE_MAX);

            // Every `u16` is the point of this test, and 196k interpreted
            // lookups is not viable under Miri. There the table boundary and
            // its neighbourhood carry the property; the exhaustive sweep runs
            // natively.
            #[cfg(miri)]
            let states = (0..64).chain(u16::MAX - 64..=u16::MAX);
            #[cfg(not(miri))]
            let states = 0..=u16::MAX;

            for state in states {
                // Indexing at all is the test: a masked index outside the
                // array would panic here rather than return.
                let masked = table.entry_masked(state);
                if (state as usize) < size {
                    // In range, it must be exactly what the checked accessor
                    // returns — masking may not perturb valid decoding.
                    assert_eq!(
                        masked,
                        table.entry(state).unwrap(),
                        "masked and checked lookups disagree at {state}"
                    );
                }
            }
        }
    }

    /// A state past the table still resolves to *some* entry rather than
    /// erroring, which is the behaviour change masking makes. Recorded so the
    /// trade is visible in the test suite and not only in a comment: the
    /// rejection such input used to get now comes from the checksum instead.
    #[test]
    fn masking_folds_an_impossible_state_rather_than_rejecting_it() {
        let table = DecodeTable::from_counts(&OF_DEFAULT, OF_DEFAULT_ACCURACY).unwrap();
        assert!(
            table.entry(32).is_err(),
            "the checked accessor still refuses"
        );
        assert_eq!(
            table.entry_masked(32),
            table.entry(0).unwrap(),
            "the masked accessor folds it back into the table"
        );
    }

    #[test]
    fn distribution_rejects_counts_that_do_not_sum() {
        // Sums to 63, not 64.
        let bad = [4i16, 3, 2, 54];
        assert!(Distribution::from_counts(&bad, 6).is_err());
    }

    #[test]
    fn distribution_rejects_excessive_accuracy_log() {
        assert!(Distribution::from_counts(&[1i16], 10).is_err());
    }

    /// Round-trip through the wire format: a hand-encoded table description
    /// must parse back to the distribution it encodes. The encoder here is
    /// written straight from the RFC's decoding rules, run in reverse, so
    /// agreement means both directions read the spec the same way.
    #[test]
    fn parses_a_hand_encoded_table_description() {
        // A tiny distribution: accuracy log 5 (table size 32), symbols 0..3
        // with counts 16, 8, 4, 4.
        let counts = [16i16, 8, 4, 4];
        let accuracy_log = 5u32;

        let mut out_bits: Vec<(u32, u32)> = Vec::new(); // (value, width)
        out_bits.push((accuracy_log - 5, 4));

        let table_size = 1i32 << accuracy_log;
        let mut remaining = table_size + 1;
        let mut threshold = table_size;
        let mut nb_bits = accuracy_log + 1;

        for &c in &counts {
            let max = (2 * threshold - 1) - remaining;
            let biased = i32::from(c) + 1;
            if biased < max {
                out_bits.push((biased as u32, nb_bits - 1));
            } else {
                let encoded = if biased >= threshold {
                    biased + max
                } else {
                    biased
                };
                out_bits.push((encoded as u32, nb_bits));
            }
            remaining -= i32::from(c).abs();
            while remaining < threshold {
                nb_bits -= 1;
                threshold >>= 1;
            }
        }

        // Pack LSB-first, matching ForwardBits.
        let mut bytes = alloc::vec![0u8; 16];
        let mut pos = 0usize;
        for (value, width) in out_bits {
            for i in 0..width {
                if (value >> i) & 1 == 1 {
                    bytes[pos / 8] |= 1 << (pos % 8);
                }
                pos += 1;
            }
        }

        let (parsed, _) = Distribution::parse(&bytes, 9, 255).unwrap();
        assert_eq!(parsed.accuracy_log(), accuracy_log);
        for (symbol, &expected) in counts.iter().enumerate() {
            assert_eq!(parsed.counts[symbol], expected, "symbol {symbol}");
        }
        DecodeTable::build(&parsed).expect("a parsed distribution must build");
    }

    #[test]
    fn parse_rejects_truncated_descriptions() {
        // Claims accuracy log 9 then runs out of bits immediately.
        assert!(Distribution::parse(&[0x04], 9, 255).is_err());
    }

    #[test]
    fn parse_rejects_excessive_accuracy_log() {
        // Low nibble 15 -> accuracy log 20, far past any legal value.
        assert!(Distribution::parse(&[0x0F, 0, 0, 0], 9, 255).is_err());
    }
}
