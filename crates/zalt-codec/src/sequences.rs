//! The sequences section of a compressed block (RFC 8878 §3.1.1.3.2).
//!
//! # Three orderings, all different, all mandatory
//!
//! This is the part of zstd most easily got wrong, because three distinct
//! orders are in play and none of them match:
//!
//! | Step | Order |
//! |---|---|
//! | State initialisation | literals-length, **offset**, match-length |
//! | Reading value bits | **offset**, match-length, literals-length |
//! | State advance | literals-length, match-length, **offset** |
//!
//! Get any of them wrong and the decoder produces plausible-looking garbage
//! rather than an error, often for thousands of bytes before anything
//! disagrees. The tests here pin each order explicitly.
//!
//! The bitstream itself is read **backwards** (see [`BackwardBits`]).

use alloc::vec::Vec;

use crate::bits::BackwardBits;
use crate::error::{Error, Result};
use crate::fse::{self, DecodeTable, Distribution, State};

/// Number of repeat offsets a decoder tracks.
pub const REP_COUNT: usize = 3;
/// Repeat offsets at the start of a frame.
pub const REP_INITIAL: [u32; REP_COUNT] = [1, 4, 8];

/// Shortest match the format can express — match-length code 0 has baseline 3.
pub const MIN_MATCH: u32 = 3;

/// How a symbol's FSE table is supplied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Use the specification's predefined distribution.
    Predefined,
    /// A single symbol, given inline.
    Rle,
    /// An FSE table description follows.
    FseCompressed,
    /// Reuse the table from a previous block.
    ///
    /// Stateful across blocks: `Repeat` in the first block of a frame has
    /// nothing to repeat and must be rejected.
    Repeat,
}

impl Mode {
    fn from_bits(bits: u8) -> Self {
        match bits {
            0 => Self::Predefined,
            1 => Self::Rle,
            2 => Self::FseCompressed,
            _ => Self::Repeat,
        }
    }
}

/// One decoded sequence: literals to emit, then a match to copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sequence {
    /// Literal bytes to copy before the match.
    pub literal_length: u32,
    /// Bytes to copy from earlier output.
    pub match_length: u32,
    /// Distance back to copy from. Always at least 1.
    pub offset: u32,
}

/// FSE tables carried across blocks within a frame.
#[derive(Debug, Clone, Default)]
pub struct Tables {
    /// Literals-length table.
    pub literals_length: DecodeTable,
    /// Offset-code table.
    pub offset: DecodeTable,
    /// Match-length table.
    pub match_length: DecodeTable,
    /// Working buffers shared by all three builds. One per decoder, not one
    /// per table: they are live only during a build, and the three builds in a
    /// block are sequential.
    pub scratch: fse::BuildScratch,
}

impl Tables {
    /// Mark every table unbuilt, keeping the allocations.
    ///
    /// What frame reset needs: a later `Repeat` must find no table, but the
    /// arrays are worth keeping — they are the same size for every frame.
    pub fn invalidate(&mut self) {
        self.literals_length.invalidate();
        self.offset.invalidate();
        self.match_length.invalidate();
    }
}

// ------------------------------------------------------------ code tables

/// Baselines for literals-length codes 16..=35. Codes below 16 are their own
/// value with no extra bits.
pub(crate) const LL_BASELINE: [u32; 20] = [
    16, 18, 20, 22, 24, 28, 32, 40, 48, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768,
    65536,
];
/// Extra bits for literals-length codes 16..=35.
pub(crate) const LL_EXTRA: [u32; 20] = [
    1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];

/// Baselines for match-length codes 32..=52. Codes below 32 are `code + 3`.
pub(crate) const ML_BASELINE: [u32; 21] = [
    35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515, 1027, 2051, 4099, 8195, 16387,
    32771, 65539,
];
/// Extra bits for match-length codes 32..=52.
pub(crate) const ML_EXTRA: [u32; 21] = [
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];

/// Baseline and extra-bit count for a literals-length code.
///
/// The range `match` looks like it should be a flat lookup table, and it was
/// tried as one: `[(u32, u32); 36]` indexed by the code. It measured **9%
/// slower** end to end, consistently, in an A/B of two binaries alternating.
/// The low codes are their own baseline with no extra bits, so the `match`
/// resolves them in a compare and a move with no memory access at all, and a
/// table turns every sequence into a load. Left as it is deliberately.
fn literals_length_code(code: u8) -> Result<(u32, u32)> {
    match code {
        0..=15 => Ok((u32::from(code), 0)),
        16..=35 => {
            let i = usize::from(code) - 16;
            Ok((LL_BASELINE[i], LL_EXTRA[i]))
        }
        _ => Err(Error::Malformed),
    }
}

/// Baseline and extra-bit count for a match-length code.
fn match_length_code(code: u8) -> Result<(u32, u32)> {
    match code {
        0..=31 => Ok((u32::from(code) + 3, 0)),
        32..=52 => {
            let i = usize::from(code) - 32;
            Ok((ML_BASELINE[i], ML_EXTRA[i]))
        }
        _ => Err(Error::Malformed),
    }
}

// ------------------------------------------------------------------ header

/// Read the sequence count (RFC 8878 §3.1.1.3.2.1).
///
/// Returns the count and bytes consumed. A leading zero byte means the block
/// has no sequences **and no symbol-modes byte** — the section ends there,
/// which is easy to miss and produces a spurious parse error if it is.
fn read_count(input: &[u8]) -> Result<(u32, usize)> {
    let b0 = *input.first().ok_or(Error::Truncated)?;
    match b0 {
        0 => Ok((0, 1)),
        1..=127 => Ok((u32::from(b0), 1)),
        128..=254 => {
            let b1 = *input.get(1).ok_or(Error::Truncated)?;
            Ok((((u32::from(b0) - 0x80) << 8) + u32::from(b1), 2))
        }
        255 => {
            let b1 = *input.get(1).ok_or(Error::Truncated)?;
            let b2 = *input.get(2).ok_or(Error::Truncated)?;
            Ok((u32::from(b1) + (u32::from(b2) << 8) + 0x7F00, 3))
        }
    }
}

/// The per-symbol constants that distinguish LL, OF and ML tables.
///
/// Grouped rather than passed loose: the three differ only in these four
/// values, and threading them individually made `resolve_table` a wall of
/// same-typed arguments where a transposed pair would compile silently.
struct SymbolSpec {
    default_counts: &'static [i16],
    default_accuracy: u32,
    max_accuracy: u32,
    max_symbol: u16,
}

const LL_SPEC: SymbolSpec = SymbolSpec {
    default_counts: &fse::LL_DEFAULT,
    default_accuracy: fse::LL_DEFAULT_ACCURACY,
    max_accuracy: fse::LL_ACCURACY_MAX,
    max_symbol: fse::LL_SYMBOL_MAX,
};

const OF_SPEC: SymbolSpec = SymbolSpec {
    default_counts: &fse::OF_DEFAULT,
    default_accuracy: fse::OF_DEFAULT_ACCURACY,
    max_accuracy: fse::OF_ACCURACY_MAX,
    max_symbol: fse::OF_SYMBOL_MAX,
};

const ML_SPEC: SymbolSpec = SymbolSpec {
    default_counts: &fse::ML_DEFAULT,
    default_accuracy: fse::ML_DEFAULT_ACCURACY,
    max_accuracy: fse::ML_ACCURACY_MAX,
    max_symbol: fse::ML_SYMBOL_MAX,
};

/// Select or build one symbol's FSE table according to its mode.
fn resolve_table<'a>(
    mode: Mode,
    spec: &SymbolSpec,
    input: &[u8],
    cursor: &mut usize,
    slot: &'a mut DecodeTable,
    scratch: &mut fse::BuildScratch,
) -> Result<&'a DecodeTable> {
    // Written into the slot that carries it across blocks, and borrowed from
    // there. It used to be built, returned by value, and then *cloned* into the
    // slot — and a `DecodeTable` is a fixed 512-entry array, so that was about
    // two kilobytes copied per table and six per block. On a 60 MB frame that
    // is lost in the noise; on the 3–12 KB documents this decoder actually
    // targets it is a large fraction of the work, and `Repeat` mode paid it
    // again to clone a table it already had.
    // Each arm builds *into* the slot's existing table rather than replacing it.
    // The slot already holds a 512-entry array from the previous block, and
    // reusing it avoids both zeroing a fresh one and moving two kilobytes in.
    // `get_or_insert_with` pays that once, on the first table of a frame.
    //
    // A build that fails must leave the slot empty rather than half-written: a
    // later `Repeat` looks for a table here, and a plausible-but-wrong one is
    // exactly the failure this decoder is most careful about elsewhere.
    match mode {
        Mode::Predefined => {
            let built = slot.from_counts_into(spec.default_counts, spec.default_accuracy, scratch);
            if built.is_err() {
                slot.invalidate();
            }
            built?;
        }
        Mode::Rle => {
            let symbol = *input.get(*cursor).ok_or(Error::Truncated)?;
            *cursor += 1;
            if u16::from(symbol) > spec.max_symbol {
                return Err(Error::Malformed);
            }
            slot.rle_into(symbol);
        }
        Mode::FseCompressed => {
            let rest = input.get(*cursor..).ok_or(Error::Truncated)?;
            let (distribution, used) =
                Distribution::parse(rest, spec.max_accuracy, spec.max_symbol)?;
            *cursor += used;
            let built = slot.build_into(&distribution, scratch);
            if built.is_err() {
                slot.invalidate();
            }
            built?;
        }
        // Keeps whatever the previous block left, which is the whole point of
        // the mode. Reusing it in place is also what makes it free.
        Mode::Repeat => {}
    }
    if slot.is_built() {
        Ok(slot)
    } else {
        Err(Error::NoPriorTable)
    }
}

/// Decode the sequences section of a compressed block.
///
/// `repeats` is carried across blocks within a frame and updated in place.
pub fn decode(
    input: &[u8],
    tables: &mut Tables,
    repeats: &mut [u32; REP_COUNT],
    out: &mut Vec<Sequence>,
    max_output: u64,
) -> Result<()> {
    let (count, mut cursor) = read_count(input)?;

    // Every sequence emits a match of at least `MIN_MATCH` bytes, so a block
    // bounded to `max_output` cannot hold more than `max_output / MIN_MATCH`
    // of them. `Number_of_Sequences` reaches ~98,000 in three bytes; without
    // this, a small block header reserves well over a megabyte of `Sequence`
    // before a single one is decoded.
    if u64::from(count) > max_output / u64::from(MIN_MATCH) {
        return Err(Error::TooLarge);
    }

    if count == 0 {
        // "there are no sequences. The sequence section stops here."
        // (RFC 8878 §3.1.1.3.2.1) — so anything after the count is extraneous
        // and the block is corrupt. Returning `Ok` here without checking is a
        // validation gap: it silently accepts a frame the reference refuses,
        // which is the direction that matters. Caught by the reference's own
        // `zeroSeq_extraneous` regression fixture.
        if cursor != input.len() {
            return Err(Error::Malformed);
        }
        // The FSE tables used in Repeat_Mode are deliberately left untouched.
        return Ok(());
    }

    let modes_byte = *input.get(cursor).ok_or(Error::Truncated)?;
    cursor += 1;
    if modes_byte & 0b11 != 0 {
        return Err(Error::Reserved);
    }
    let ll_mode = Mode::from_bits(modes_byte >> 6);
    let of_mode = Mode::from_bits((modes_byte >> 4) & 0b11);
    let ml_mode = Mode::from_bits((modes_byte >> 2) & 0b11);

    // Table descriptions appear in this order: literals-length, offset,
    // match-length.
    // Split the borrow: the three slots are distinct fields, so each can be
    // written and then borrowed independently.
    let Tables {
        literals_length,
        offset,
        match_length,
        scratch,
    } = tables;
    let ll_table = resolve_table(
        ll_mode,
        &LL_SPEC,
        input,
        &mut cursor,
        literals_length,
        scratch,
    )?;
    let of_table = resolve_table(of_mode, &OF_SPEC, input, &mut cursor, offset, scratch)?;
    let ml_table = resolve_table(ml_mode, &ML_SPEC, input, &mut cursor, match_length, scratch)?;

    out.reserve(count as usize);

    let payload = input.get(cursor..).ok_or(Error::Truncated)?;
    let mut bits = BackwardBits::new(payload)?;

    // Initialisation order: literals-length, offset, match-length.
    let mut ll_state = State::new(ll_table, &mut bits)?;
    let mut of_state = State::new(of_table, &mut bits)?;
    let mut ml_state = State::new(ml_table, &mut bits)?;

    for decoded in 1..=count as usize {
        // One lookup per state, used for both the symbol now and the advance
        // below. The state does not change in between, so the second lookup
        // the previous version performed was fetching the identical entry.
        let ll_entry = ll_state.entry(ll_table);
        let ml_entry = ml_state.entry(ml_table);
        let of_entry = of_state.entry(of_table);
        let (ll_code, ml_code, of_code) = (ll_entry.symbol, ml_entry.symbol, of_entry.symbol);

        let (ll_base, ll_bits) = literals_length_code(ll_code)?;
        let (ml_base, ml_bits) = match_length_code(ml_code)?;
        if u32::from(of_code) > 31 {
            return Err(Error::Malformed);
        }

        // Value-bit order: offset, match-length, literals-length. Three
        // consecutive reads compose into one, exactly as the transition bits
        // below do — but these three can total 63 bits, past what one cached
        // word can serve, so the wide path is guarded and the rare case falls
        // back to reading them separately.
        let of_nbits = u32::from(of_code);
        let total = of_nbits + ml_bits + ll_bits;
        let (of_raw, ml_raw, ll_raw) = if total <= crate::bits::WIDE_MAX {
            let raw = bits.read_wide(total);
            (
                (raw >> (ml_bits + ll_bits)) as u32,
                ((raw >> ll_bits) & ((1u64 << ml_bits) - 1)) as u32,
                (raw & ((1u64 << ll_bits) - 1)) as u32,
            )
        } else {
            (bits.read(of_nbits), bits.read(ml_bits), bits.read(ll_bits))
        };

        let offset_value = (1u32 << of_code)
            .checked_add(of_raw)
            .ok_or(Error::Malformed)?;
        let match_length = ml_base + ml_raw;
        let literal_length = ll_base + ll_raw;
        bits.check()?;

        let offset = resolve_offset(offset_value, literal_length == 0, repeats)?;
        out.push(Sequence {
            literal_length,
            match_length,
            offset,
        });

        // Advance order: literals-length, match-length, offset. Skipped after
        // the final sequence, whose transition bits were never written.
        if decoded < count as usize {
            // Three consecutive reads from one stream are one read and a
            // split. `read` takes the first bit read as the most significant,
            // so `read(a + b + c)` places the three fields in order from the
            // top — the property `backward_consecutive_reads_compose` pins.
            //
            // Each accuracy log is at most 9, so the total is at most 27 bits
            // and always fits a `u32`. Saving two of the six bit reads a
            // sequence performs is worth doing in the hottest loop here.
            let n_ll = u32::from(ll_entry.nb_bits);
            let n_ml = u32::from(ml_entry.nb_bits);
            let n_of = u32::from(of_entry.nb_bits);
            debug_assert!(n_ll + n_ml + n_of <= 32);
            let raw = bits.read(n_ll + n_ml + n_of);

            // A zero-width field masks to zero, which is what a zero-bit read
            // returned, so no branch is needed for it.
            let of_add = raw & ((1u32 << n_of) - 1);
            let ml_add = (raw >> n_of) & ((1u32 << n_ml) - 1);
            let ll_add = raw >> (n_of + n_ml);

            ll_state.advance_by(ll_entry, ll_add as u16);
            ml_state.advance_by(ml_entry, ml_add as u16);
            of_state.advance_by(of_entry, of_add as u16);
            bits.check()?;
        }
    }

    // "At the end, the bitstream shall be entirely consumed; otherwise, the
    // bitstream is considered corrupted." (RFC 8878 §3.1.1.3.2.1.2)
    //
    // Without this a truncated or over-long bitstream still decodes the
    // declared number of sequences and reports success, because the reader
    // returns zero bits past the end rather than failing.
    if !bits.is_finished() {
        return Err(Error::Malformed);
    }

    Ok(())
}

/// Turn an `Offset_Value` into a real offset, applying the repeat-offset rules.
///
/// Values 1–3 select a remembered offset rather than encoding one. The
/// `literals_length == 0` case shifts which one — the single most commonly
/// botched rule in the format, because a decoder that ignores it still produces
/// output, just wrong output, and only on blocks that happen to contain a
/// zero-literal sequence.
#[inline(always)]
/// Resolve an `Offset_Value` against the repeat table, updating it.
///
/// `pub(crate)` so the **encoder** can use it as its own oracle. §4 calls the
/// `literals_length == 0` shift "the single most commonly botched part of the
/// format"; rather than write an inverse of this rule and hope the two agree,
/// `encode::sequences` asks *this function* which offset value produces the
/// offset it wants. The two directions cannot disagree about a rule they share.
pub(crate) fn resolve_offset(
    offset_value: u32,
    ll_zero: bool,
    repeats: &mut [u32; REP_COUNT],
) -> Result<u32> {
    if offset_value > REP_COUNT as u32 {
        let offset = offset_value - REP_COUNT as u32;
        repeats[2] = repeats[1];
        repeats[1] = repeats[0];
        repeats[0] = offset;
        return Ok(offset);
    }

    // 1..=3 index the repeat table, shifted by one when there are no literals.
    let index = offset_value - 1 + u32::from(ll_zero);
    if index == 0 {
        // Reusing the most recent offset changes nothing.
        return Ok(repeats[0]);
    }

    let offset = if index as usize == REP_COUNT {
        // The escape: "the one before the most recent", encoded as rep[0] - 1.
        repeats[0].checked_sub(1).filter(|v| *v > 0)
    } else {
        Some(repeats[index as usize])
    }
    .ok_or(Error::Malformed)?;

    if index >= 2 {
        repeats[2] = repeats[1];
    }
    repeats[1] = repeats[0];
    repeats[0] = offset;
    Ok(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_count_encodings() {
        assert_eq!(read_count(&[0]).unwrap(), (0, 1));
        assert_eq!(read_count(&[1]).unwrap(), (1, 1));
        assert_eq!(read_count(&[127]).unwrap(), (127, 1));
        // 128..254: two bytes.
        assert_eq!(read_count(&[0x80, 0x00]).unwrap(), (0, 2));
        assert_eq!(read_count(&[0x81, 0x02]).unwrap(), (258, 2));
        // 255: three bytes, biased by 0x7F00.
        assert_eq!(read_count(&[255, 0x00, 0x00]).unwrap(), (0x7F00, 3));
        assert_eq!(read_count(&[255, 0x01, 0x01]).unwrap(), (0x7F00 + 257, 3));
    }

    #[test]
    fn zero_sequences_consumes_only_one_byte() {
        // The modes byte is absent entirely; a decoder expecting it would read
        // whatever follows the section.
        let (count, used) = read_count(&[0, 0xFF, 0xFF]).unwrap();
        assert_eq!(count, 0);
        assert_eq!(used, 1);
    }

    #[test]
    fn literals_length_codes_match_the_spec() {
        assert_eq!(literals_length_code(0).unwrap(), (0, 0));
        assert_eq!(literals_length_code(15).unwrap(), (15, 0));
        assert_eq!(literals_length_code(16).unwrap(), (16, 1));
        assert_eq!(literals_length_code(19).unwrap(), (22, 1));
        assert_eq!(literals_length_code(20).unwrap(), (24, 2));
        assert_eq!(literals_length_code(24).unwrap(), (48, 4));
        assert_eq!(literals_length_code(35).unwrap(), (65536, 16));
        assert!(literals_length_code(36).is_err());
    }

    #[test]
    fn match_length_codes_match_the_spec() {
        assert_eq!(match_length_code(0).unwrap(), (3, 0), "minimum match is 3");
        assert_eq!(match_length_code(31).unwrap(), (34, 0));
        assert_eq!(match_length_code(32).unwrap(), (35, 1));
        assert_eq!(match_length_code(42).unwrap(), (99, 5));
        assert_eq!(match_length_code(52).unwrap(), (65539, 16));
        assert!(match_length_code(53).is_err());
    }

    /// Code baselines must be contiguous: each code's range has to start
    /// exactly where the previous one ended, or some lengths are unreachable
    /// and others ambiguous.
    #[test]
    fn code_ranges_tile_without_gaps_or_overlap() {
        let mut expected = 0u32;
        for code in 0..=35u8 {
            let (base, bits) = literals_length_code(code).unwrap();
            assert_eq!(base, expected, "literals-length code {code}");
            expected = base + (1u32 << bits);
        }

        let mut expected = 3u32;
        for code in 0..=52u8 {
            let (base, bits) = match_length_code(code).unwrap();
            assert_eq!(base, expected, "match-length code {code}");
            expected = base + (1u32 << bits);
        }
    }

    #[test]
    fn modes_decode_from_the_right_bit_positions() {
        // LL bits 7-6, OF bits 5-4, ML bits 3-2.
        let byte = (2u8 << 6) | (1 << 4) | (3 << 2);
        assert_eq!(Mode::from_bits(byte >> 6), Mode::FseCompressed);
        assert_eq!(Mode::from_bits((byte >> 4) & 3), Mode::Rle);
        assert_eq!(Mode::from_bits((byte >> 2) & 3), Mode::Repeat);
    }

    // ------------------------------------------------------ repeat offsets

    #[test]
    fn regular_offset_shifts_the_repeat_table() {
        let mut reps = REP_INITIAL;
        // Offset_Value 10 -> real offset 7.
        assert_eq!(resolve_offset(10, false, &mut reps).unwrap(), 7);
        assert_eq!(reps, [7, 1, 4]);
    }

    #[test]
    fn repeat_one_with_literals_reuses_most_recent_and_changes_nothing() {
        let mut reps = [5u32, 9, 13];
        assert_eq!(resolve_offset(1, false, &mut reps).unwrap(), 5);
        assert_eq!(reps, [5, 9, 13], "reusing the newest offset is a no-op");
    }

    #[test]
    fn repeat_two_promotes_the_second_offset() {
        let mut reps = [5u32, 9, 13];
        assert_eq!(resolve_offset(2, false, &mut reps).unwrap(), 9);
        assert_eq!(reps, [9, 5, 13]);
    }

    #[test]
    fn repeat_three_promotes_the_third_offset() {
        let mut reps = [5u32, 9, 13];
        assert_eq!(resolve_offset(3, false, &mut reps).unwrap(), 13);
        assert_eq!(reps, [13, 5, 9]);
    }

    /// **The rule everyone gets wrong.** With no literals, `Offset_Value` 1
    /// means the *second* remembered offset, not the first — every index
    /// shifts up by one.
    #[test]
    fn zero_literals_shifts_every_repeat_index() {
        let mut reps = [5u32, 9, 13];
        assert_eq!(
            resolve_offset(1, true, &mut reps).unwrap(),
            9,
            "with no literals, value 1 selects the second offset"
        );
        assert_eq!(reps, [9, 5, 13]);

        let mut reps = [5u32, 9, 13];
        assert_eq!(resolve_offset(2, true, &mut reps).unwrap(), 13);

        // Value 3 with no literals reaches the escape: rep[0] - 1.
        let mut reps = [5u32, 9, 13];
        assert_eq!(resolve_offset(3, true, &mut reps).unwrap(), 4);
        assert_eq!(reps, [4, 5, 9]);
    }

    /// The escape cannot produce offset zero — that would be a match copying
    /// from itself.
    #[test]
    fn escape_from_offset_one_is_rejected() {
        let mut reps = [1u32, 4, 8];
        assert!(resolve_offset(3, true, &mut reps).is_err());
    }

    #[test]
    fn initial_repeats_are_one_four_eight() {
        assert_eq!(REP_INITIAL, [1, 4, 8]);
    }
}
