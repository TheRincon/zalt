//! The literals and sequences sections of a compressed block.
//!
//! ## Codes are derived from the decoder's tables, not transcribed
//!
//! libzstd carries `LL_Code[64]` and `ML_Code[128]` lookup tables mapping a
//! value to its code. Transcribing those is two more chances to make the
//! mistake §10 records against `ML_DEFAULT`, where a table that summed
//! correctly still mapped states to the wrong symbols and every self-consistency
//! check passed. So the code for a value is *searched* out of the baseline
//! tables the decoder already uses: slower, and correct by construction,
//! because the two directions cannot disagree about a table they share.
//!
//! ## The bitstream order
//!
//! The decoder reads backwards, so the encoder writes the sequences in reverse
//! and flushes the states last. §4 records the read order; this is its inverse,
//! and it follows libzstd's `ZSTD_encodeSequences_body` step for step:
//!
//! ```text
//! init states from the LAST sequence      (ML, OF, LL)
//! its value bits                          (LL, ML, OF)
//! for each earlier sequence, last to first:
//!     state transitions                   (OF, ML, LL)
//!     value bits                          (LL, ML, OF)
//! flush states                            (ML, OF, LL)
//! ```
//!
//! Every one of those triples is in the opposite order to the decoder's, which
//! is the only thing making them look inconsistent with §4.

use alloc::vec::Vec;

use super::bitstream::BitWriter;
// Two modules named `fse`: the decoder's, which owns the predefined
// distributions and the symbol limits, and the encoder's, which owns table
// construction. Both are needed here, so the encoder's is aliased.
use super::fse as enc;
use super::fse::{CState, CTable};
use super::huffman;
use super::match_finder::Sequence;
use crate::fse;
use crate::sequences::{LL_BASELINE, LL_EXTRA, ML_BASELINE, ML_EXTRA};

/// Split a value into its code and the extra bits carried beside it.
///
/// `direct` covers the low codes that are their own value, `baselines` and
/// `extra` the rest. Returns `(code, extra_value, extra_bits)`.
fn split(value: u32, direct: u32, offset: u8, baselines: &[u32], extra: &[u32]) -> (u8, u32, u32) {
    if value < direct {
        return (value as u8, 0, 0);
    }
    // The largest baseline not above the value. Baselines ascend, so the last
    // one that fits is the code.
    let mut index = 0usize;
    for (i, &base) in baselines.iter().enumerate() {
        if base <= value {
            index = i;
        } else {
            break;
        }
    }
    (offset + index as u8, value - baselines[index], extra[index])
}

/// Code, extra value and extra bit count for a literals length.
pub fn literals_length_code(value: u32) -> (u8, u32, u32) {
    // Codes 0..=15 are their own value; 16..=35 are baselined.
    split(value, 16, 16, &LL_BASELINE, &LL_EXTRA)
}

/// Code, extra value and extra bit count for a match length.
pub fn match_length_code(value: u32) -> (u8, u32, u32) {
    // Codes 0..=31 are `value - 3`; 32..=52 are baselined.
    if value < 35 {
        debug_assert!(value >= 3, "the format's shortest match is three bytes");
        return ((value - 3) as u8, 0, 0);
    }
    let (code, extra_value, extra_bits) = split(value, 0, 32, &ML_BASELINE, &ML_EXTRA);
    (code, extra_value, extra_bits)
}

/// Split an `Offset_Value` into its code and extra bits.
///
/// The code is the position of the value's highest set bit, so it doubles as
/// the number of extra bits — which is why the decoder reads `of_code` of them.
fn split_offset_value(value: u32) -> (u8, u32, u32) {
    let code = 31 - value.leading_zeros();
    (code as u8, value - (1 << code), code)
}

/// Write the literals section, choosing the cheapest representation.
///
/// Three forms, in the order they are tried:
///
/// - **RLE** (type 1) when every literal is the same byte. One byte on the
///   wire, and the only form that can describe a single-symbol alphabet at all
///   — a Huffman table needs two.
/// - **Compressed** (type 2) when a Huffman code pays for itself. The header,
///   the weight description and the jump table together cost tens of bytes, so
///   the comparison below has to clear them, not merely beat raw.
/// - **Raw** (type 0) otherwise, and whenever anything above declines. Raw is
///   never wrong, which is what makes every refusal in the Huffman path safe.
pub fn write_literals(out: &mut Vec<u8>, literals: &[u8]) {
    if literals.is_empty() {
        write_raw_literals(out, literals);
        return;
    }
    if literals.iter().all(|b| *b == literals[0]) {
        write_rle_literals(out, literals[0], literals.len() as u32);
        return;
    }
    if let Some(section) = compressed_literals(literals) {
        out.extend_from_slice(&section);
        return;
    }
    write_raw_literals(out, literals);
}

/// Build a compressed literals section, or `None` if it would not pay.
fn compressed_literals(literals: &[u8]) -> Option<Vec<u8>> {
    // The four-stream form splits the literals into quarters, and a section too
    // small to split is not worth a Huffman table anyway.
    if literals.len() < 64 {
        return None;
    }

    let counts = huffman::counts(literals);
    let code = huffman::build(&counts)?;

    let mut body = Vec::new();
    if !huffman::write_weights(&mut body, &code) {
        return None;
    }
    let streams = huffman::encode_four_streams(&code, literals)?;
    body.extend_from_slice(&streams);

    let regenerated = literals.len() as u32;
    let compressed = body.len() as u32;

    // Header widths: 10 bits each up to 1023, then 14, then 18.
    let (size_format, header_len) = if regenerated < 1024 && compressed < 1024 {
        (0b01u32, 3usize)
    } else if regenerated < 16_384 && compressed < 16_384 {
        (0b10, 4)
    } else if regenerated < 262_144 && compressed < 262_144 {
        (0b11, 5)
    } else {
        return None;
    };

    // Must actually beat raw, headers included.
    if header_len + body.len() >= literals.len() + raw_header_len(regenerated) {
        return None;
    }

    let bits = match size_format {
        0b01 => 10,
        0b10 => 14,
        _ => 18,
    };
    let value = 0b10u64
        | (u64::from(size_format) << 2)
        | (u64::from(regenerated) << 4)
        | (u64::from(compressed) << (4 + bits));

    let mut out = Vec::with_capacity(header_len + body.len());
    out.extend_from_slice(&value.to_le_bytes()[..header_len]);
    out.extend_from_slice(&body);
    Some(out)
}

fn raw_header_len(size: u32) -> usize {
    if size < 32 {
        1
    } else if size < 4096 {
        2
    } else {
        3
    }
}

/// Write an RLE literals section: one byte, repeated.
fn write_rle_literals(out: &mut Vec<u8>, byte: u8, size: u32) {
    if size < 32 {
        out.push(1 | (size << 3) as u8);
    } else if size < 4096 {
        let v = 0b01_01u32 | (size << 4);
        out.extend_from_slice(&v.to_le_bytes()[..2]);
    } else {
        let v = 0b11_01u32 | (size << 4);
        out.extend_from_slice(&v.to_le_bytes()[..3]);
    }
    out.push(byte);
}

/// Write a raw literals section (RFC 8878 §3.1.1.3.1).
///
/// Type 0, so the literals are copied verbatim. The header is one, two or three
/// bytes depending on the run; `Size_Format` is one bit when even and two when
/// odd, which is why `00` and `10` mean the same thing.
pub fn write_raw_literals(out: &mut Vec<u8>, literals: &[u8]) {
    let size = literals.len() as u32;
    if size < 32 {
        out.push((size << 3) as u8);
    } else if size < 4096 {
        let v = 0b01_00u32 | (size << 4);
        out.extend_from_slice(&v.to_le_bytes()[..2]);
    } else {
        let v = 0b11_00u32 | (size << 4);
        out.extend_from_slice(&v.to_le_bytes()[..3]);
    }
    out.extend_from_slice(literals);
}

/// Write `Number_of_Sequences` (RFC 8878 §3.1.1.3.2.1).
fn write_count(out: &mut Vec<u8>, count: u32) {
    if count < 128 {
        out.push(count as u8);
    } else if count <= 32_511 {
        out.push((128 + (count >> 8)) as u8);
        out.push((count & 0xFF) as u8);
    } else {
        out.push(255);
        let rest = count - 0x7F00;
        out.push((rest & 0xFF) as u8);
        out.push((rest >> 8) as u8);
    }
}

/// One sequence, reduced to what the bitstream actually carries.
struct Coded {
    ll: u8,
    ll_value: u32,
    ll_bits: u32,
    ml: u8,
    ml_value: u32,
    ml_bits: u32,
    of: u8,
    of_value: u32,
    of_bits: u32,
}

impl Coded {
    fn of(s: &Sequence) -> Self {
        let (ll, ll_value, ll_bits) = literals_length_code(s.literal_length);
        let (ml, ml_value, ml_bits) = match_length_code(s.match_length);
        let (of, of_value, of_bits) = split_offset_value(s.offset_value);
        Self {
            ll,
            ll_value,
            ll_bits,
            ml,
            ml_value,
            ml_bits,
            of,
            of_value,
            of_bits,
        }
    }

    /// The value bits, in the order the decoder reads them reversed.
    fn write_values(&self, w: &mut BitWriter) {
        w.add_bits(u64::from(self.ll_value), self.ll_bits);
        w.add_bits(u64::from(self.ml_value), self.ml_bits);
        w.add_bits(u64::from(self.of_value), self.of_bits);
    }
}

/// A table the sequence bitstream will be coded against.
struct Chosen {
    /// `Symbol_Compression_Modes` value: 0 predefined, 2 FSE-compressed.
    mode: u8,
    /// The description to write, empty for predefined.
    description: Vec<u8>,
    table: CTable,
}

/// Fit a table to this block's own code distribution, or keep the predefined one.
///
/// Predefined tables cost nothing to describe and are tuned for "typical"
/// data; a fitted table costs a description and pays for it whenever the block
/// is not typical. Which wins is decided by measurement rather than estimate —
/// see [`write_sequences`].
fn fit(
    counts: &[u32],
    max_accuracy: u32,
    default_norm: &'static [i16],
    default_accuracy: u32,
) -> Option<Chosen> {
    let last = counts.iter().rposition(|&c| c != 0)?;
    let total: usize = counts.iter().map(|&c| c as usize).sum();
    let accuracy = enc::optimal_table_log(max_accuracy, total, last as u32);

    let norm = enc::normalize(&counts[..=last], accuracy)?;
    // A description must end on a non-zero count, or it leaves a run length
    // owed that the reader will take from the next symbol's bits.
    let end = norm.iter().rposition(|&n| n != 0)?;
    let norm = &norm[..=end];

    let mut description = Vec::new();
    enc::write_table_description(&mut description, norm, accuracy);
    let table = CTable::build(norm, accuracy);
    let _ = (default_norm, default_accuracy);
    Some(Chosen {
        mode: 2,
        description,
        table,
    })
}

fn predefined(norm: &'static [i16], accuracy: u32) -> Chosen {
    Chosen {
        mode: 0,
        description: Vec::new(),
        table: CTable::build(norm, accuracy),
    }
}

/// Encode the bitstream for one choice of the three tables.
fn encode_bitstream(coded: &[Coded], ll: &CTable, ml: &CTable, of: &CTable) -> Vec<u8> {
    // A sequence costs three state transitions of at most ten bits each and
    // three value fields of at most sixteen, sixteen and thirty-one — under
    // twelve bytes. This is a ceiling, not an estimate: the stream cannot
    // exceed it, so the writer never reallocates, and the block's own size
    // caps how far the over-reservation can go.
    let mut w = BitWriter::with_capacity(coded.len() * 12 + 16);
    let last = coded.last().expect("non-empty");

    // The last sequence seeds the states and contributes only its value bits;
    // the decoder reads those first, immediately after the initial states.
    let mut ml_state = CState::init(ml, last.ml);
    let mut of_state = CState::init(of, last.of);
    let mut ll_state = CState::init(ll, last.ll);
    last.write_values(&mut w);

    for s in coded.iter().rev().skip(1) {
        of_state.encode(&mut w, of, s.of);
        ml_state.encode(&mut w, ml, s.ml);
        ll_state.encode(&mut w, ll, s.ll);
        s.write_values(&mut w);
    }

    ml_state.flush(&mut w);
    of_state.flush(&mut w);
    ll_state.flush(&mut w);
    w.close()
}

/// Assemble the part of the section that follows the sequence count.
fn assemble(coded: &[Coded], ll: &Chosen, of: &Chosen, ml: &Chosen) -> Vec<u8> {
    let mut out = Vec::new();
    // LL in bits 6-7, OF in 4-5, ML in 2-3; the low two bits are reserved and
    // must be zero.
    out.push((ll.mode << 6) | (of.mode << 4) | (ml.mode << 2));
    // Descriptions appear literals-length, offset, match-length — not the
    // order the states are initialised in, and not the order the bitstream
    // uses either.
    out.extend_from_slice(&ll.description);
    out.extend_from_slice(&of.description);
    out.extend_from_slice(&ml.description);
    out.extend_from_slice(&encode_bitstream(coded, &ll.table, &ml.table, &of.table));
    out
}

/// Write the sequences section, fitting tables to the block where that is
/// smaller than using the predefined ones.
///
/// **Both candidates are built and the smaller kept**, rather than choosing
/// from an entropy estimate as libzstd does. An estimate has to model the
/// description's cost as well as the bitstream's, and being wrong is invisible
/// — it costs ratio, silently, which is the failure mode this encoder keeps
/// tripping over. Encoding twice is a straightforward cost for an answer that
/// cannot be worse than the predefined tables it replaces.
///
/// The refinement left on the table is choosing *per table* rather than for all
/// three at once: the modes byte has independent fields, so a block could fit
/// its literals lengths and keep the predefined offsets. That is eight
/// candidates instead of two, and worth doing once there is a measurement
/// asking for it.
pub fn write_sequences(out: &mut Vec<u8>, sequences: &[Sequence]) {
    write_count(out, sequences.len() as u32);
    if sequences.is_empty() {
        // "A zero byte means the block has no sequences and no symbol-modes
        // byte — the section ends there."
        return;
    }

    let coded: Vec<Coded> = sequences.iter().map(Coded::of).collect();

    let mut ll_counts = [0u32; 36];
    let mut ml_counts = [0u32; 53];
    let mut of_counts = [0u32; 32];
    for c in &coded {
        ll_counts[usize::from(c.ll)] += 1;
        ml_counts[usize::from(c.ml)] += 1;
        of_counts[usize::from(c.of)] += 1;
    }

    let baseline = assemble(
        &coded,
        &predefined(&fse::LL_DEFAULT, fse::LL_DEFAULT_ACCURACY),
        &predefined(&fse::OF_DEFAULT, fse::OF_DEFAULT_ACCURACY),
        &predefined(&fse::ML_DEFAULT, fse::ML_DEFAULT_ACCURACY),
    );

    let fitted = fit(
        &ll_counts,
        fse::LL_ACCURACY_MAX,
        &fse::LL_DEFAULT,
        fse::LL_DEFAULT_ACCURACY,
    )
    .zip(fit(
        &of_counts,
        fse::OF_ACCURACY_MAX,
        &fse::OF_DEFAULT,
        fse::OF_DEFAULT_ACCURACY,
    ))
    .zip(fit(
        &ml_counts,
        fse::ML_ACCURACY_MAX,
        &fse::ML_DEFAULT,
        fse::ML_DEFAULT_ACCURACY,
    ))
    .map(|((ll, of), ml)| assemble(&coded, &ll, &of, &ml));

    match fitted {
        Some(f) if f.len() < baseline.len() => out.extend_from_slice(&f),
        _ => out.extend_from_slice(&baseline),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Codes must invert the decoder's own baseline lookup.
    ///
    /// The decoder turns a code into `(baseline, extra_bits)`; this turns a
    /// value into `(code, extra_value, extra_bits)`. Composing them must give
    /// the value back, for every value, which is a stronger check than
    /// comparing against a transcribed table would be.
    #[test]
    fn literals_length_codes_invert_the_decoder() {
        for value in (0..=16u32).chain([
            17, 31, 32, 63, 64, 127, 128, 255, 256, 511, 512, 16_383, 65_535, 131_071,
        ]) {
            let (code, extra, bits) = literals_length_code(value);
            assert!(code <= 35, "value {value} produced code {code}");
            let base = if code < 16 {
                u32::from(code)
            } else {
                LL_BASELINE[code as usize - 16]
            };
            let expected_bits = if code < 16 {
                0
            } else {
                LL_EXTRA[code as usize - 16]
            };
            assert_eq!(bits, expected_bits, "value {value}");
            assert_eq!(base + extra, value, "value {value} via code {code}");
            assert!(extra < (1u64 << bits) as u32 || bits == 0, "value {value}");
        }
    }

    #[test]
    fn match_length_codes_invert_the_decoder() {
        for value in (3..=35u32).chain([36, 40, 63, 64, 127, 128, 1000, 65_538, 131_074]) {
            let (code, extra, bits) = match_length_code(value);
            assert!(code <= 52, "value {value} produced code {code}");
            let base = if code < 32 {
                u32::from(code) + 3
            } else {
                ML_BASELINE[code as usize - 32]
            };
            let expected_bits = if code < 32 {
                0
            } else {
                ML_EXTRA[code as usize - 32]
            };
            assert_eq!(bits, expected_bits, "value {value}");
            assert_eq!(base + extra, value, "value {value} via code {code}");
        }
    }

    #[test]
    fn a_literal_offset_splits_the_way_the_decoder_reassembles_it() {
        for offset in (1..=40u32).chain([255, 256, 4095, 4096, 65_535, 131_072]) {
            let (code, extra, bits) = split_offset_value(offset + 3);
            assert_eq!(bits, u32::from(code), "the code is its own bit count");
            // The decoder computes `(1 << of_code) + of_raw`, then resolves it.
            let value = (1u32 << code) + extra;
            assert_eq!(value - 3, offset, "offset {offset}");
        }
    }

    #[test]
    fn a_short_literals_run_uses_a_one_byte_header() {
        let mut out = Vec::new();
        write_raw_literals(&mut out, &[1, 2, 3]);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0] & 0b11, 0, "raw type");
        assert_eq!(out[0] >> 3, 3, "5-bit size");
    }

    #[test]
    fn sequence_counts_round_trip_through_the_decoder() {
        for count in [0u32, 1, 127, 128, 129, 255, 256, 32_511, 32_512, 70_000] {
            let mut out = Vec::new();
            write_count(&mut out, count);
            // Mirror of the decoder's reader, which is not public.
            let got = match out[0] {
                b0 if b0 < 128 => u32::from(b0),
                255 => u32::from(out[1]) + (u32::from(out[2]) << 8) + 0x7F00,
                b0 => ((u32::from(b0) - 0x80) << 8) + u32::from(out[1]),
            };
            assert_eq!(got, count);
        }
    }
}
