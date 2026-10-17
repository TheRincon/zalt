// SPDX-License-Identifier: (Unlicense OR MIT OR Apache-2.0) AND BSD-3-Clause
//
// Derived in part from zstd 1.5.7, `lib/compress/fse_compress.c`:
//   Copyright (c) Meta Platforms, Inc. and affiliates. All rights reserved.
// The derived parts are under zstd's BSD license, in `LICENSE-ZSTD` at this
// crate's root. The module docs below say what follows zstd and how closely.

//! FSE encoding tables and states.
//!
//! The encode-side counterpart to [`crate::fse`]. Building an *encoding* table
//! is a different construction from building a decoding one, and this follows
//! libzstd's `FSE_buildCTable_wksp` rather than deriving it independently:
//! the symbol-spreading walk has to produce byte-identical state assignments to
//! the decoder's, and "byte-identical to a specific implementation" is not a
//! thing a second reading of the specification reliably lands on.
//!
//! The test that matters is at the bottom and does not check any of this
//! directly: it encodes a symbol sequence and decodes it with
//! [`crate::fse`], which is the only property worth having.

use alloc::vec;
use alloc::vec::Vec;

use super::bitstream::BitWriter;

/// The step used to spread symbols across the table.
///
/// `(size >> 1) + (size >> 3) + 3`, an odd number coprime with the table size,
/// so repeatedly adding it modulo the size visits every slot exactly once.
fn table_step(table_size: u32) -> u32 {
    (table_size >> 1) + (table_size >> 3) + 3
}

/// Position of the highest set bit, or 0 for an input of 0.
pub(super) fn highbit32(v: u32) -> u32 {
    31 - v.max(1).leading_zeros()
}

/// A built FSE encoding table.
pub struct CTable {
    pub(super) table_log: u32,
    /// Next state for each (symbol-ordered) table slot.
    next_state: Vec<u16>,
    /// Per symbol, the packed bit-count delta.
    delta_nb_bits: Vec<u32>,
    /// Per symbol, the offset into `next_state`. Signed: the first symbols
    /// index below their own cumulative position.
    delta_find_state: Vec<i32>,
}

impl CTable {
    /// Build an encoding table from a normalised distribution.
    ///
    /// `normalised` is the same representation the decoder reads: a count per
    /// symbol summing to `1 << table_log`, where `-1` marks a
    /// less-than-one-in-the-table symbol that is placed at the top of the state
    /// range rather than spread through it.
    pub fn build(normalised: &[i16], table_log: u32) -> Self {
        let table_size = 1u32 << table_log;
        let table_mask = table_size - 1;
        let step = table_step(table_size);
        let max_sv1 = normalised.len();

        // Cumulative start positions, and the low-probability symbols placed
        // from the top of the table downwards.
        let mut cumul = vec![0u32; max_sv1 + 1];
        let mut table_symbol = vec![0u8; table_size as usize];
        let mut high_threshold = table_size - 1;
        for u in 1..=max_sv1 {
            if normalised[u - 1] == -1 {
                cumul[u] = cumul[u - 1] + 1;
                table_symbol[high_threshold as usize] = (u - 1) as u8;
                high_threshold -= 1;
            } else {
                cumul[u] = cumul[u - 1] + normalised[u - 1].max(0) as u32;
            }
        }
        cumul[max_sv1] = table_size + 1;

        // Spread the rest, skipping the low-probability region at the top.
        let mut position = 0u32;
        for (symbol, &freq) in normalised.iter().enumerate() {
            for _ in 0..freq.max(0) {
                table_symbol[position as usize] = symbol as u8;
                position = (position + step) & table_mask;
                while position > high_threshold {
                    position = (position + step) & table_mask;
                }
            }
        }
        debug_assert_eq!(position, 0, "the walk must visit every slot exactly once");

        // For each slot, the state a symbol lands on when it is encoded there.
        let mut next_state = vec![0u16; table_size as usize];
        let mut cursor = cumul.clone();
        for u in 0..table_size {
            let s = table_symbol[u as usize] as usize;
            next_state[cursor[s] as usize] = (table_size + u) as u16;
            cursor[s] += 1;
        }

        // The per-symbol transform. `delta_nb_bits` packs a bit count in its
        // high half and a bias in its low half, so the encoder gets the width
        // of the next write from a single addition and a shift.
        let mut delta_nb_bits = vec![0u32; max_sv1];
        let mut delta_find_state = vec![0i32; max_sv1];
        let mut total = 0i32;
        for (s, &count) in normalised.iter().enumerate() {
            match count {
                0 => {
                    // Unreachable for a symbol never emitted, but filled in so
                    // the table is total and a bug shows up as a wrong width
                    // rather than as reading uninitialised memory.
                    delta_nb_bits[s] = ((table_log + 1) << 16) - table_size;
                }
                -1 | 1 => {
                    delta_nb_bits[s] = (table_log << 16) - table_size;
                    delta_find_state[s] = total - 1;
                    total += 1;
                }
                n => {
                    let n = n as u32;
                    let max_bits_out = table_log - highbit32(n - 1);
                    let min_state_plus = n << max_bits_out;
                    delta_nb_bits[s] = (max_bits_out << 16) - min_state_plus;
                    delta_find_state[s] = total - n as i32;
                    total += n as i32;
                }
            }
        }

        Self {
            table_log,
            next_state,
            delta_nb_bits,
            delta_find_state,
        }
    }

    fn transition(&self, state: u32, symbol: u8) -> (u32, u32) {
        let s = symbol as usize;
        let nb_bits_out = (state.wrapping_add(self.delta_nb_bits[s])) >> 16;
        let index = ((state >> nb_bits_out) as i64) + i64::from(self.delta_find_state[s]);
        debug_assert!(index >= 0 && (index as usize) < self.next_state.len());
        (nb_bits_out, u32::from(self.next_state[index as usize]))
    }
}

/// An in-flight FSE encoder state.
pub struct CState {
    value: u32,
    table_log: u32,
}

impl CState {
    /// Start a state on its first symbol.
    ///
    /// The first symbol encoded is the *last* one the decoder will read, and it
    /// contributes no bits of its own — the state is seeded directly from it,
    /// which is why the decoder's initial state read yields it back.
    pub fn init(table: &CTable, symbol: u8) -> Self {
        let s = symbol as usize;
        let delta = table.delta_nb_bits[s];
        let nb_bits_out = (delta.wrapping_add(1 << 15)) >> 16;
        let value = (nb_bits_out << 16).wrapping_sub(delta);
        let index = ((value >> nb_bits_out) as i64) + i64::from(table.delta_find_state[s]);
        debug_assert!(index >= 0 && (index as usize) < table.next_state.len());
        Self {
            value: u32::from(table.next_state[index as usize]),
            table_log: table.table_log,
        }
    }

    /// Encode one symbol, emitting its transition bits.
    pub fn encode(&mut self, w: &mut BitWriter, table: &CTable, symbol: u8) {
        let (nb_bits_out, next) = table.transition(self.value, symbol);
        w.add_bits(u64::from(self.value), nb_bits_out);
        self.value = next;
    }

    /// Write the final state, which the decoder reads first.
    pub fn flush(self, w: &mut BitWriter) {
        w.add_bits(u64::from(self.value), self.table_log);
    }
}

// ------------------------------------------------- the table description

/// A forward, LSB-first bit writer.
///
/// Distinct from [`BitWriter`], which the *backward* reader consumes. An FSE
/// table description is read forward by [`crate::bits::ForwardBits`], so it is
/// written forward too, with no end marker — the description's length is known
/// from the count it encodes rather than from a sentinel.
struct ForwardBitWriter {
    out: Vec<u8>,
    acc: u32,
    bits: u32,
}

impl ForwardBitWriter {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            acc: 0,
            bits: 0,
        }
    }

    fn add(&mut self, value: u32, nb_bits: u32) {
        debug_assert!(nb_bits <= 24);
        if nb_bits == 0 {
            return;
        }
        self.acc |= (value & ((1u32 << nb_bits) - 1)) << self.bits;
        self.bits += nb_bits;
        while self.bits >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.bits -= 8;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bits > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

/// The accuracy log libzstd would choose for this many symbols.
///
/// `FSE_optimalTableLog` with the sequence encoder's `minus = 2`. A table
/// larger than the data justifies costs header bytes that buy nothing, and one
/// too small cannot separate the symbols — this lands between, and is
/// transcribed rather than invented because the trade-off has been tuned
/// against far more corpora than are in this repository.
pub fn optimal_table_log(max_log: u32, count: usize, max_symbol: u32) -> u32 {
    const MIN_TABLE_LOG: u32 = 5;
    if count <= 1 {
        return MIN_TABLE_LOG.min(max_log);
    }
    let max_bits_src = highbit32((count - 1) as u32).saturating_sub(2);
    let min_bits_src = highbit32(count as u32) + 1;
    let min_bits_symbols = highbit32(max_symbol) + 2;
    let min_bits = min_bits_src.min(min_bits_symbols);

    let mut log = max_log;
    if max_bits_src < log {
        log = max_bits_src;
    }
    if min_bits > log {
        log = min_bits;
    }
    log.clamp(MIN_TABLE_LOG, max_log)
}

/// Scale raw counts to a distribution summing to `1 << accuracy_log`.
///
/// Deliberately simple: proportional scaling, a floor of one for any symbol
/// that occurs, then the remainder pushed onto or taken from the largest.
/// libzstd's `FSE_normalizeCount` is more careful about which symbol absorbs
/// the rounding error, which is worth a fraction of a percent — and this runs
/// over an alphabet of at most thirteen weights, where there is very little
/// error to place.
///
/// Returns `None` if the alphabet cannot fit the table at all.
pub fn normalize(counts: &[u32], accuracy_log: u32) -> Option<Vec<i16>> {
    let target = 1i32 << accuracy_log;
    let total: u64 = counts.iter().map(|&c| u64::from(c)).sum();
    if total == 0 {
        return None;
    }
    let present = counts.iter().filter(|&&c| c > 0).count() as i32;
    if present == 0 || present > target {
        return None;
    }

    let mut norm = vec![0i16; counts.len()];
    let mut sum = 0i32;
    for (i, &count) in counts.iter().enumerate() {
        if count == 0 {
            continue;
        }
        let scaled = (u64::from(count) * target as u64 / total).max(1) as i32;
        norm[i] = scaled.min(target) as i16;
        sum += i32::from(norm[i]);
    }

    // Settle the total on the largest symbol, which can absorb the adjustment
    // with the least proportional distortion.
    while sum != target {
        let pick = (0..norm.len())
            .filter(|&i| norm[i] > 0)
            .filter(|&i| sum <= target || norm[i] > 1)
            .max_by_key(|&i| norm[i])?;
        if sum > target {
            norm[pick] -= 1;
            sum -= 1;
        } else {
            norm[pick] += 1;
            sum += 1;
        }
    }
    Some(norm)
}

/// Write an FSE table description (RFC 8878 §4.1.1).
///
/// The inverse of `Distribution::parse`, and structurally libzstd's
/// `FSE_writeNCount`: counts are coded in a width that narrows as the remaining
/// probability mass shrinks, and a zero count is followed by a two-bit run
/// length repeated while it reads 3.
///
/// `norm` must end on a non-zero count. A trailing zero would leave the writer
/// owing a run length it never emits, and the reader would consume the next
/// symbol's bits as one.
pub fn write_table_description(out: &mut Vec<u8>, norm: &[i16], accuracy_log: u32) {
    debug_assert!(matches!(norm.last(), Some(n) if *n != 0));
    let mut w = ForwardBitWriter::new();
    w.add(accuracy_log - 5, 4);

    let table_size = 1i32 << accuracy_log;
    let mut remaining = table_size + 1;
    let mut threshold = table_size;
    let mut nb_bits = accuracy_log + 1;
    let mut symbol = 0usize;
    let mut previous_is_zero = false;

    while symbol < norm.len() && remaining > 1 {
        if previous_is_zero {
            let start = symbol;
            let mut next = symbol;
            while next < norm.len() && norm[next] == 0 {
                next += 1;
            }
            if next == norm.len() {
                break;
            }
            let mut run = next - start;
            while run >= 3 {
                w.add(3, 2);
                run -= 3;
            }
            w.add(run as u32, 2);
            symbol = next;
        }

        let count = i32::from(norm[symbol]);
        symbol += 1;
        let max = (2 * threshold - 1) - remaining;
        remaining -= count.abs();

        // Biased by one so that -1 is expressible, then displaced above the
        // threshold so the narrow and wide forms do not overlap.
        let mut coded = count + 1;
        if coded >= threshold {
            coded += max;
        }
        w.add(
            coded as u32,
            if coded < max { nb_bits - 1 } else { nb_bits },
        );
        previous_is_zero = coded == 1;

        while remaining < threshold {
            nb_bits -= 1;
            threshold >>= 1;
        }
    }

    out.extend_from_slice(&w.finish());
}

/// Encode a symbol run with two interleaved states.
///
/// The shape `decode_fse_weights` expects. Both states are **seeded from the
/// last two symbols** rather than started blank: a seeded state emits no bits,
/// and the decoder's initial state read recovers that symbol directly. Starting
/// them blank was tried first, and produced a stream the decoder read one
/// symbol too far — every symbol was right except the highest, which came back
/// as its neighbour, because the extra symbol shifted the weight array and so
/// the whole Huffman table by one.
///
/// Copied from `FSE_compress_usingCTable` rather than re-derived. The
/// odd-length prologue and the `& 2` join exist so the main loop can run four
/// at a time with the alternation landing where the decoder looks for it, and
/// that pairing is correct for exactly one arrangement.
pub fn encode_interleaved(table: &CTable, symbols: &[u8]) -> Option<Vec<u8>> {
    let n = symbols.len();
    if n <= 2 {
        // Two symbols would be entirely state initialisation, with no bits to
        // read back; libzstd refuses this too.
        return None;
    }

    let mut w = BitWriter::new();
    let mut i = n;
    let (mut c1, mut c2);

    if n & 1 != 0 {
        i -= 1;
        c1 = CState::init(table, symbols[i]);
        i -= 1;
        c2 = CState::init(table, symbols[i]);
        i -= 1;
        c1.encode(&mut w, table, symbols[i]);
    } else {
        i -= 1;
        c2 = CState::init(table, symbols[i]);
        i -= 1;
        c1 = CState::init(table, symbols[i]);
    }

    // The two seeded symbols are already accounted for.
    if (n - 2) & 2 != 0 {
        i -= 1;
        c2.encode(&mut w, table, symbols[i]);
        i -= 1;
        c1.encode(&mut w, table, symbols[i]);
    }

    while i > 0 {
        i -= 1;
        c2.encode(&mut w, table, symbols[i]);
        i -= 1;
        c1.encode(&mut w, table, symbols[i]);
        i -= 1;
        c2.encode(&mut w, table, symbols[i]);
        i -= 1;
        c1.encode(&mut w, table, symbols[i]);
    }

    c2.flush(&mut w);
    c1.flush(&mut w);
    Some(w.close())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BackwardBits;
    use crate::fse;

    /// Encode a run of symbols and decode them with the decoder's own tables.
    ///
    /// This is the only assertion about any of the above worth making. The
    /// construction is intricate and its intermediate values mean nothing on
    /// their own; what matters is that a decoder written independently of it,
    /// and already run against the reference corpus, reads back what went in.
    fn round_trip(normalised: &[i16], accuracy: u32, symbols: &[u8]) {
        let ctable = CTable::build(normalised, accuracy);
        let dtable = fse::DecodeTable::from_counts(normalised, accuracy)
            .expect("the decoder accepts it too");

        let mut w = BitWriter::new();
        // Written last-to-first, because the decoder reads the stream backwards.
        let mut state = CState::init(&ctable, *symbols.last().expect("non-empty"));
        for &symbol in symbols.iter().rev().skip(1) {
            state.encode(&mut w, &ctable, symbol);
        }
        state.flush(&mut w);
        let bytes = w.close();

        let mut r = BackwardBits::new(&bytes).expect("a stream we closed");
        let mut decoded = Vec::new();
        let mut ds = fse::State::new(&dtable, &mut r).expect("initial state");
        for i in 0..symbols.len() {
            let entry = ds.entry(&dtable);
            decoded.push(entry.symbol);
            if i + 1 < symbols.len() {
                let add = r.read(u32::from(entry.nb_bits));
                ds.advance_by(entry, add as u16);
            }
        }
        assert_eq!(decoded, symbols, "round trip through the decoder's table");
        assert!(!r.exhausted());
    }

    #[test]
    fn the_predefined_literals_table_round_trips() {
        let symbols: Vec<u8> = (0..36u8).chain((0..36u8).rev()).collect();
        round_trip(&fse::LL_DEFAULT, fse::LL_DEFAULT_ACCURACY, &symbols);
    }

    #[test]
    fn the_predefined_match_length_table_round_trips() {
        let symbols: Vec<u8> = (0..53u8).chain(core::iter::repeat_n(7, 40)).collect();
        round_trip(&fse::ML_DEFAULT, fse::ML_DEFAULT_ACCURACY, &symbols);
    }

    #[test]
    fn the_predefined_offset_table_round_trips() {
        // Includes the low-probability symbols at the top of the range, which
        // are placed by a different branch of the spreading walk than the rest.
        let symbols: Vec<u8> = (0..29u8).chain([28, 27, 26, 25, 24, 0, 1]).collect();
        round_trip(&fse::OF_DEFAULT, fse::OF_DEFAULT_ACCURACY, &symbols);
    }

    #[test]
    fn a_single_symbol_round_trips() {
        // One sequence writes no transition bits at all: the state is seeded
        // and immediately flushed. An off-by-one in the loop shows up here and
        // nowhere else.
        round_trip(&fse::LL_DEFAULT, fse::LL_DEFAULT_ACCURACY, &[5]);
        round_trip(&fse::OF_DEFAULT, fse::OF_DEFAULT_ACCURACY, &[28]);
    }

    #[test]
    fn the_spreading_walk_covers_every_slot() {
        // `build` debug-asserts this, but only for the tables it is handed.
        for (norm, log) in [
            (&fse::LL_DEFAULT[..], fse::LL_DEFAULT_ACCURACY),
            (&fse::ML_DEFAULT[..], fse::ML_DEFAULT_ACCURACY),
            (&fse::OF_DEFAULT[..], fse::OF_DEFAULT_ACCURACY),
        ] {
            let t = CTable::build(norm, log);
            assert_eq!(t.next_state.len(), 1 << log);
            // Every slot holds a state in the upper half of the range, which is
            // what "tableSize + u" means; a zero would be a slot never written.
            assert!(t.next_state.iter().all(|s| u32::from(*s) >= (1 << log)));
        }
    }
}
