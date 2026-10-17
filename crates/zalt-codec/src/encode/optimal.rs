// SPDX-License-Identifier: (Unlicense OR MIT OR Apache-2.0) AND BSD-3-Clause
//
// Derived in part from zstd 1.5.7, `lib/compress/zstd_opt.c`:
//   Copyright (c) Meta Platforms, Inc. and affiliates. All rights reserved.
// The derived parts are under zstd's BSD license, in `LICENSE-ZSTD` at this
// crate's root. The module docs below say what follows zstd and how closely.

//! The price model behind optimal parsing (levels 16 and up).
//!
//! An optimal parser chooses, among every way of covering a stretch of input
//! with literals and matches, the one that costs fewest bits. That needs a
//! *price* for each choice before any of it is encoded, so this module estimates
//! one from symbol statistics: a symbol seen with frequency `f` out of `sum`
//! costs about `log2(sum / f)` bits, plus whatever extra bits its code carries.
//!
//! ## Transcribed from libzstd's `zstd_opt.c`, deliberately
//!
//! The weight function, the base frequencies for a first block, the literal
//! update step of 2, and the one-fifth-of-a-bit handicap per match all follow
//! libzstd 1.5.7, read from the vendored source. Unlike the decoder's tables,
//! these are *heuristics* — nothing reads them back — so transcribing them risks
//! only ratio, and starting from the values a mature encoder converged on is a
//! better prior than inventing new ones.
//!
//! ## Prices are allowed to be wrong
//!
//! That is the property the whole design rests on. A price only decides which
//! path the parser prefers. Which `Offset_Value` is written for the chosen path
//! is still decided by the decoder's own `resolve_offset`, and the sequences go
//! through the same writer every other level uses. A bad estimate costs bytes;
//! it cannot cost correctness.
//!
//! ## Fixed point, because `core` has no logarithm
//!
//! This crate is `no_std` and float-free. Prices are in 1/256ths of a bit, and
//! `log2` is approximated as the position of the highest set bit plus a linear
//! fraction — libzstd's `ZSTD_fracWeight`, exact at powers of two and within
//! about 0.09 bits between them.

use crate::sequences::{LL_BASELINE, LL_EXTRA, ML_BASELINE, ML_EXTRA};

// ------------------------------------------------------------------ codes

/// Literals-length code by searching the decoder's own baselines.
///
/// `const`, so the small lookup table below is computed at build time *from the
/// decoder's table* rather than transcribed — the M5 rule for this module's
/// neighbour [`super::sequences`], and for the same reason: the two directions
/// cannot disagree about a table they share.
const fn ll_code_by_search(length: u32) -> u8 {
    if length < 16 {
        return length as u8;
    }
    let mut code = 0;
    let mut i = 0;
    while i < LL_BASELINE.len() {
        if LL_BASELINE[i] <= length {
            code = i;
        }
        i += 1;
    }
    (16 + code) as u8
}

/// Match-length code by searching the decoder's own baselines.
const fn ml_code_by_search(match_length: u32) -> u8 {
    if match_length < 35 {
        return (match_length - 3) as u8;
    }
    let mut code = 0;
    let mut i = 0;
    while i < ML_BASELINE.len() {
        if ML_BASELINE[i] <= match_length {
            code = i;
        }
        i += 1;
    }
    (32 + code) as u8
}

const LL_SMALL: [u8; 64] = {
    let mut t = [0u8; 64];
    let mut i = 0;
    while i < 64 {
        t[i] = ll_code_by_search(i as u32);
        i += 1;
    }
    t
};

/// Indexed by `match_length - 3`.
const ML_SMALL: [u8; 128] = {
    let mut t = [0u8; 128];
    let mut i = 0;
    while i < 128 {
        t[i] = ml_code_by_search(i as u32 + 3);
        i += 1;
    }
    t
};

fn highbit(v: u32) -> u32 {
    31 - v.max(1).leading_zeros()
}

/// Literals-length code, in constant time.
///
/// Past 63 every code's baseline is a power of two, so the code is the length's
/// highest bit plus a fixed delta — libzstd's `ZSTD_LLcode`. That is a claim
/// about the decoder's table, and the tests check it at every length a block
/// can hold rather than trusting it.
///
/// Clamped at [`LL_MAX`], the longest literals length the format can express:
/// one more would compute code 36, which does not exist.
pub fn ll_code(length: u32) -> u8 {
    let length = length.min(LL_MAX);
    if length < 64 {
        LL_SMALL[length as usize]
    } else {
        (highbit(length) + 19) as u8
    }
}

/// Match-length code, in constant time. libzstd's `ZSTD_MLcode`, checked the
/// same way.
pub fn ml_code(match_length: u32) -> u8 {
    let base = match_length - 3;
    if base < 128 {
        ML_SMALL[base as usize]
    } else {
        (highbit(base) + 36) as u8
    }
}

/// Longest literals length the format can express: code 35's baseline, 65 536,
/// plus sixteen extra bits.
///
/// A block is at most 131 072 bytes and a sequence's literals are followed by a
/// match of at least three, so no real sequence comes near it. The clamp exists
/// so that the price table cannot be indexed out of range even if that
/// argument is ever wrong — the parser prices long runs of literals one byte at
/// a time, and a panic there would be reachable from content.
pub const LL_MAX: u32 = 65_536 + (1 << 16) - 1;

fn ll_extra(code: u8) -> u32 {
    if code < 16 {
        0
    } else {
        LL_EXTRA[usize::from(code) - 16]
    }
}

fn ml_extra(code: u8) -> u32 {
    if code < 32 {
        0
    } else {
        ML_EXTRA[usize::from(code) - 32]
    }
}

/// Fractional bits in a price.
const ACCURACY: u32 = 8;

/// One bit, in price units.
pub const BIT: u32 = 1 << ACCURACY;

/// Fixed-point `log2(raw + 1) + 1`, in price units.
///
/// The `+ 1` on both sides cancels in every price, which is always a difference
/// of two weights — `weight(sum) - weight(freq)` approximates
/// `log2(sum / freq)`.
pub fn weight(raw: u32) -> u32 {
    // u64 so a large statistic cannot overflow the shift; libzstd asserts the
    // same bound instead.
    let stat = u64::from(raw) + 1;
    let hb = 63 - stat.leading_zeros();
    let whole = u64::from(hb) * u64::from(BIT);
    let fraction = (stat << ACCURACY) >> hb;
    (whole + fraction) as u32
}

const LL_CODES: usize = 36;
const ML_CODES: usize = 53;
const OF_CODES: usize = 32;

/// libzstd's starting literal-length frequencies for a first block: short runs
/// are expected, a run of zero most of all.
const BASE_LL: [u32; LL_CODES] = [
    4, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1,
];

/// libzstd's starting offset-code frequencies: repeat codes (0 and 1) are
/// common, and so are offsets in the tens to low thousands.
const BASE_OF: [u32; OF_CODES] = [
    6, 2, 1, 1, 2, 3, 4, 4, 4, 3, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
];

/// Each literal emitted bumps its frequency by this, so literal statistics
/// adapt faster than the sequence codes'.
const LITERAL_STEP: u32 = 2;

/// Symbol statistics the prices are derived from.
#[derive(Clone)]
pub struct Stats {
    lit: [u32; 256],
    ll: [u32; LL_CODES],
    ml: [u32; ML_CODES],
    of: [u32; OF_CODES],
}

impl Stats {
    /// Statistics for a frame's first block, before anything has been parsed.
    ///
    /// Literal frequencies come from the block's own bytes. That over-counts
    /// bytes that will end up inside matches, which is libzstd's approximation
    /// too, and is the reason the top levels parse the first block twice — the
    /// second time from the literals the first parse actually left.
    pub fn first_block(block: &[u8]) -> Self {
        let mut stats = Self {
            lit: [0; 256],
            ll: BASE_LL,
            ml: [1; ML_CODES],
            of: BASE_OF,
        };
        stats.seed_literals(block);
        stats
    }

    /// Carry statistics into the next block.
    ///
    /// The sequence codes keep what the previous block taught them, scaled down
    /// so the new block's own sequences soon outweigh them. Literals are
    /// re-seeded from the new block, which may be a different kind of content.
    pub fn next_block(&mut self, block: &[u8]) {
        for f in self.ll.iter_mut().chain(&mut self.ml).chain(&mut self.of) {
            *f = (*f >> 1) + 1;
        }
        self.seed_literals(block);
    }

    fn seed_literals(&mut self, block: &[u8]) {
        let mut counts = [0u32; 256];
        for &b in block {
            counts[usize::from(b)] += 1;
        }
        // Scale so the table describes proportions, not this block's length:
        // a 128 KB block should not drown out the updates the parse makes.
        let shift = (usize::BITS - block.len().leading_zeros()).saturating_sub(10);
        for (f, c) in self.lit.iter_mut().zip(counts) {
            *f = 1 + (c >> shift);
        }
    }

    /// Learn from one emitted sequence.
    pub fn record(&mut self, literals: &[u8], match_length: u32, offset_value: u32) {
        for &b in literals {
            self.lit[usize::from(b)] += LITERAL_STEP;
        }
        self.ll[usize::from(ll_code(literals.len() as u32))] += 1;
        self.ml[usize::from(ml_code(match_length))] += 1;
        self.of[offset_code(offset_value) as usize] += 1;
    }
}

/// The offset code for an `Offset_Value`: the position of its highest bit,
/// which is also the number of extra bits it carries.
fn offset_code(offset_value: u32) -> u32 {
    31 - offset_value.max(1).leading_zeros()
}

/// Prices, precomputed from one [`Stats`] snapshot.
///
/// Rebuilt between stretches of parsing rather than on every lookup: a price
/// table is a few hundred subtractions, and the parser consults it hundreds of
/// times per byte.
pub struct Prices {
    lit: [u32; 256],
    ll: [u32; LL_CODES],
    ml: [u32; ML_CODES],
    of: [u32; OF_CODES],
}

impl Prices {
    /// Prices for the statistics as they stand.
    pub fn new(stats: &Stats) -> Self {
        fn table<const N: usize>(freq: &[u32; N]) -> [u32; N] {
            let base = weight(freq.iter().sum());
            let mut out = [0u32; N];
            for (p, &f) in out.iter_mut().zip(freq) {
                *p = base.saturating_sub(weight(f));
            }
            out
        }
        let mut lit = table(&stats.lit);
        // At least one bit: libzstd caps the subtracted weight the same way, so
        // a dominant byte is never free and long literal runs are never priced
        // below what any Huffman code could achieve.
        for p in &mut lit {
            *p = (*p).max(BIT);
        }
        Self {
            lit,
            ll: table(&stats.ll),
            ml: table(&stats.ml),
            of: table(&stats.of),
        }
    }

    /// One literal byte.
    pub fn literal(&self, byte: u8) -> u32 {
        self.lit[usize::from(byte)]
    }

    /// A literals-length field of `length`: its code's price plus its extra
    /// bits.
    pub fn literal_length(&self, length: u32) -> u32 {
        let code = ll_code(length);
        ll_extra(code) * BIT + self.ll[usize::from(code)]
    }

    /// A match: its length's code and extra bits, its offset's code and extra
    /// bits, and a fifth of a bit.
    ///
    /// The fifth is libzstd's: it makes the parser prefer fewer sequences when
    /// two paths are otherwise level, which costs nothing in size and saves the
    /// decoder a sequence.
    pub fn sequence(&self, match_length: u32, offset_value: u32) -> u32 {
        let code = ml_code(match_length);
        let of_code = offset_code(offset_value);
        ml_extra(code) * BIT
            + self.ml[usize::from(code)]
            + of_code * BIT
            + self.of[of_code as usize]
            + BIT / 5
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::sequences::{literals_length_code, match_length_code};

    /// The constant-time codes must agree with the decoder-derived search at
    /// every length a block can produce. The highest-bit shortcut is a claim
    /// that every baseline past the small tables is a power of two; this is
    /// where that claim is held to account, value by value.
    #[test]
    fn a_literals_length_past_the_format_limit_clamps_rather_than_overflowing() {
        assert_eq!(ll_code(LL_MAX + 1), ll_code(LL_MAX));
        assert_eq!(ll_code(u32::MAX), 35);
    }

    #[test]
    fn constant_time_codes_agree_with_the_decoder_tables_everywhere() {
        // Every value natively; a stride under Miri, which would take minutes.
        let step = if cfg!(miri) { 97 } else { 1 };
        // To the format's limit and no further: 131 072 has no literals-length
        // code at all (code 35 tops out at 131 071), and is where this test
        // first failed — the formula said 36, a code that does not exist.
        for length in (0..=LL_MAX).step_by(step) {
            let (code, _, extra) = literals_length_code(length);
            assert_eq!(ll_code(length), code, "literals length {length}");
            assert_eq!(ll_extra(code), extra, "literals length {length}");
        }
        for length in (3..=131_074u32).step_by(step) {
            let (code, _, extra) = match_length_code(length);
            assert_eq!(ml_code(length), code, "match length {length}");
            assert_eq!(ml_extra(code), extra, "match length {length}");
        }
    }

    /// The weight is `log2` in fixed point: exact at powers of two, monotone,
    /// and within about a tenth of a bit between them.
    #[test]
    fn weight_approximates_log2() {
        // weight(x - 1) = log2(x) + 1 bits, exactly, when x is a power of two.
        for k in 0..20u32 {
            assert_eq!(weight((1 << k) - 1), (k + 1) * BIT, "x = 2^{k}");
        }
        // log2(3) + 1 = 2.585 bits = 661.8 units; the linear fraction gives 640.
        assert!(weight(2).abs_diff(662) <= BIT / 10, "{}", weight(2));
        // log2(1000) + 1 = 10.966 bits = 2807.2 units.
        assert!(weight(999).abs_diff(2807) <= BIT / 10, "{}", weight(999));
        let mut previous = 0;
        for raw in 0..5000 {
            assert!(weight(raw) >= previous, "not monotone at {raw}");
            previous = weight(raw);
        }
    }

    /// A frequent symbol must be cheaper than a rare one, and nothing is free.
    #[test]
    fn common_symbols_are_cheaper_and_literals_cost_at_least_a_bit() {
        let mut block = alloc::vec![b'a'; 4000];
        block.extend_from_slice(b"xyz");
        let prices = Prices::new(&Stats::first_block(&block));
        assert!(prices.literal(b'a') < prices.literal(b'x'));
        assert!(prices.literal(b'a') >= BIT);
        // A byte never seen is never cheaper than one seen once, and is still
        // finite — the parser must be able to price a literal it has no
        // statistics for. Not strictly dearer: seeding scales counts down, and
        // on a small block once and never round to the same frequency, as they
        // do in libzstd.
        assert!(prices.literal(0xFE) >= prices.literal(b'x'));
        assert!(prices.literal(0xFE) <= 24 * BIT);
    }

    /// Recording a sequence makes its codes cheaper next time.
    #[test]
    fn recording_lowers_the_price_of_what_was_recorded() {
        let mut stats = Stats::first_block(b"some ordinary text to seed literal statistics");
        let before = Prices::new(&stats).sequence(40, 1000 + 3);
        for _ in 0..50 {
            stats.record(b"ab", 40, 1000 + 3);
        }
        let after = Prices::new(&stats).sequence(40, 1000 + 3);
        assert!(after < before, "{after} should be below {before}");
    }

    /// Extra bits are charged in full: a longer offset in the same code is the
    /// same price, a longer code costs at least its extra bit more.
    #[test]
    fn offsets_pay_their_extra_bits() {
        let prices = Prices::new(&Stats::first_block(b"x"));
        let small = prices.sequence(10, 64 + 3);
        let large = prices.sequence(10, 65_536 + 3);
        // Code 16 against code 6: ten more extra bits, and code 16 is the rarer
        // of the two in the base frequencies.
        assert!(large >= small + 10 * BIT, "{small} {large}");
    }
}
