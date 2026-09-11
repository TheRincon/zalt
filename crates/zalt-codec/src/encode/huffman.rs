//! Huffman-coded literals (RFC 8878 §3.1.1.3.1.1).
//!
//! Raw literals cost a byte each, and §8 says this content is
//! literal-dominated, so this is where the gap against the reference mostly
//! lives.
//!
//! ## Three things have to agree, and only one of them is Huffman
//!
//! 1. **Code lengths**, limited to 11 bits, from the literal frequencies.
//! 2. **The weight description** the decoder rebuilds the table from. Weights
//!    are `tableLog + 1 - nbBits`, written 4 bits each, and the *last* symbol's
//!    weight is omitted — the decoder derives it as whatever makes the total a
//!    power of two. Writing it anyway produces a table that is wrong by one
//!    symbol and decodes to plausible nonsense.
//! 3. **Canonical code values**, assigned by the `valPerRank` walk below. This
//!    is transcribed from libzstd rather than derived, because "canonical" does
//!    not pin the order on its own and the decoder's table was built to match
//!    libzstd's choice.
//!
//! ## Length limiting
//!
//! `tableLog` may not exceed 11, so a naive Huffman tree over skewed
//! frequencies has to be flattened. The repair here is zlib's: overflowing
//! leaves are pulled up to the limit and the excess is paid for by pushing
//! shallower leaves down, two at a time, which preserves the Kraft equality the
//! decoder checks. The alternative — package-merge — is optimal and much more
//! code, and §10 says ratio work comes after correctness.

use alloc::vec;
use alloc::vec::Vec;

use super::bitstream::BitWriter;
use super::fse;
use crate::huf::TABLELOG_MAX;

/// Streams a compressed literals section is split across.
pub const STREAMS: usize = 4;

/// A built Huffman code: a length and a value per symbol.
pub struct Code {
    /// Bits per symbol; zero for symbols that do not occur.
    lengths: [u8; 256],
    values: [u16; 256],
    /// The largest symbol with a non-zero weight.
    max_symbol: usize,
    table_log: u32,
}

impl Code {
    /// Bits this code would spend on `literals`, excluding all headers.
    ///
    /// Only the tests consult this: the section writer compares finished
    /// sizes rather than an estimate, because the weight description and jump
    /// table are tens of bytes and an estimate that ignored them would choose
    /// Huffman for sections it makes larger.
    #[cfg(test)]
    pub fn encoded_bits(&self, counts: &[u32; 256]) -> u64 {
        counts
            .iter()
            .enumerate()
            .map(|(s, &n)| u64::from(n) * u64::from(self.lengths[s]))
            .sum()
    }
}

/// Count how often each byte occurs.
pub fn counts(literals: &[u8]) -> [u32; 256] {
    let mut counts = [0u32; 256];
    for &b in literals {
        counts[usize::from(b)] += 1;
    }
    counts
}

/// Build a length-limited Huffman code, or `None` if one cannot apply.
///
/// `None` for fewer than two distinct symbols: the weight description cannot
/// express a single-symbol alphabet — its weights would sum to zero and the
/// decoder refuses that — and such literals belong in an RLE section anyway.
pub fn build(counts: &[u32; 256]) -> Option<Code> {
    let present: Vec<usize> = (0..256).filter(|&s| counts[s] > 0).collect();
    if present.len() < 2 {
        return None;
    }

    let lengths = code_lengths(counts, &present)?;
    let table_log = u32::from(*lengths.iter().max().expect("non-empty"));
    debug_assert!(table_log <= TABLELOG_MAX);

    let mut all_lengths = [0u8; 256];
    for (&symbol, &len) in present.iter().zip(&lengths) {
        all_lengths[symbol] = len;
    }

    // Canonical values, exactly as libzstd assigns them: a starting value per
    // rank, taken from the longest codes upwards, then handed out in symbol
    // order within each rank.
    let mut nb_per_rank = [0u16; (TABLELOG_MAX + 2) as usize];
    for &symbol in &present {
        nb_per_rank[usize::from(all_lengths[symbol])] += 1;
    }
    let mut val_per_rank = [0u16; (TABLELOG_MAX + 2) as usize];
    let mut min = 0u16;
    for n in (1..=table_log as usize).rev() {
        val_per_rank[n] = min;
        min += nb_per_rank[n];
        min >>= 1;
    }
    let mut values = [0u16; 256];
    for symbol in 0..256 {
        let len = usize::from(all_lengths[symbol]);
        if len > 0 {
            values[symbol] = val_per_rank[len];
            val_per_rank[len] += 1;
        }
    }

    let max_symbol = *present.last().expect("non-empty");

    // The decoder derives the last symbol's weight from the Kraft remainder.
    // If that identity does not hold, the table it builds is not the one used
    // here, so refusing is the only safe answer — and it falls back to raw
    // literals, which are never wrong.
    let total: u32 = present
        .iter()
        .map(|&s| 1u32 << (table_log + 1 - u32::from(all_lengths[s])) >> 1)
        .sum();
    if total != (1u32 << table_log) {
        return None;
    }

    Some(Code {
        lengths: all_lengths,
        values,
        max_symbol,
        table_log,
    })
}

/// Huffman code lengths for the present symbols, limited to [`TABLELOG_MAX`].
///
/// Returned in the same order as `present`.
fn code_lengths(counts: &[u32; 256], present: &[usize]) -> Option<Vec<u8>> {
    let n = present.len();
    // A tree over `n` leaves has at most `2n - 1` nodes. Leaves occupy
    // `0..n`; internal nodes are appended.
    let mut weight: Vec<u64> = present.iter().map(|&s| u64::from(counts[s])).collect();
    let mut parent: Vec<Option<usize>> = vec![None; n];
    let mut active: Vec<usize> = (0..n).collect();

    while active.len() > 1 {
        // Two smallest. Quadratic in the alphabet, which is at most 256, and
        // this runs once per block.
        let mut lowest = 0usize;
        let mut second = 1usize;
        if weight[active[second]] < weight[active[lowest]] {
            core::mem::swap(&mut lowest, &mut second);
        }
        for i in 2..active.len() {
            let w = weight[active[i]];
            if w < weight[active[lowest]] {
                second = lowest;
                lowest = i;
            } else if w < weight[active[second]] {
                second = i;
            }
        }

        let (a, b) = (active[lowest], active[second]);
        let node = weight.len();
        weight.push(weight[a] + weight[b]);
        parent.push(None);
        parent[a] = Some(node);
        parent[b] = Some(node);

        // Remove the two merged entries and add the new node.
        let (hi, lo) = if lowest > second {
            (lowest, second)
        } else {
            (second, lowest)
        };
        active.swap_remove(hi);
        active.swap_remove(lo);
        active.push(node);
    }

    // Depth of each leaf is its code length.
    let mut lengths = vec![0u8; n];
    for (i, slot) in lengths.iter_mut().enumerate() {
        let mut depth = 0u32;
        let mut cursor = i;
        while let Some(p) = parent[cursor] {
            depth += 1;
            cursor = p;
            if depth > 64 {
                return None;
            }
        }
        *slot = depth as u8;
    }

    limit_lengths(&mut lengths, counts, present);
    Some(lengths)
}

/// Force every code within [`TABLELOG_MAX`], keeping the tree exactly full.
///
/// The decoder derives the last symbol's weight from the Kraft remainder and
/// refuses a remainder that is not a power of two, so "exactly full" is not a
/// nicety — it is the difference between a table it can rebuild and one it
/// rejects.
///
/// zlib's repair was tried first and does not transfer: it assumes the surplus
/// after clamping is exactly `overflow / 2` deep-tree slots, which holds for the
/// way zlib limits depth *during* construction and not for a tree built without
/// a limit and clamped afterwards. Kraft came out wrong and `build` refused its
/// own table. This accounts for the sum directly instead, which is slower, has
/// no precondition, and can be asserted.
fn limit_lengths(lengths: &mut [u8], counts: &[u32; 256], present: &[usize]) {
    let max = TABLELOG_MAX as u8;
    for len in lengths.iter_mut() {
        *len = (*len).clamp(1, max);
    }

    let target: u64 = 1u64 << max;
    let kraft = |lengths: &[u8]| -> u64 { lengths.iter().map(|&len| 1u64 << (max - len)).sum() };

    // Over-full: too many short codes. Lengthen the least valuable symbol that
    // still has room, which costs one extra bit per occurrence.
    while kraft(lengths) > target {
        let victim = (0..lengths.len())
            .filter(|&i| lengths[i] < max)
            .min_by_key(|&i| (counts[present[i]], core::cmp::Reverse(present[i])));
        match victim {
            Some(i) => lengths[i] += 1,
            // Every code is already at the limit and the tree is still
            // over-full, which cannot happen for 256 or fewer symbols at
            // tableLog 11. Bail rather than spin.
            None => return,
        }
    }

    // Under-full: there is slack, so the most valuable symbol that can afford a
    // shorter code should take it. Shortening a code of length `l` adds
    // `2^(max - l)` to the sum, so it is only allowed while that still fits.
    loop {
        let slack = target - kraft(lengths);
        let candidate = (0..lengths.len())
            .filter(|&i| lengths[i] > 1 && (1u64 << (max - lengths[i])) <= slack)
            .max_by_key(|&i| (counts[present[i]], core::cmp::Reverse(present[i])));
        match candidate {
            Some(i) => lengths[i] -= 1,
            None => break,
        }
    }

    debug_assert_eq!(
        kraft(lengths),
        target,
        "the repaired tree is not exactly full"
    );
}

/// The weight of each symbol below the largest.
///
/// The largest symbol's own weight is omitted throughout: the decoder derives
/// it from the Kraft remainder, and writing it anyway yields a table wrong by
/// one symbol that still decodes, to nonsense.
fn weight_sequence(code: &Code) -> Vec<u8> {
    (0..code.max_symbol)
        .map(|symbol| {
            let len = u32::from(code.lengths[symbol]);
            if len == 0 {
                0
            } else {
                (code.table_log + 1 - len) as u8
            }
        })
        .collect()
}

/// Accuracy log for the weight distribution.
///
/// `HUF_WEIGHT_ACCURACY_MAX` is 6, and the weight alphabet has at most twelve
/// members, so the largest permitted table is also comfortably the best one:
/// 64 slots over twelve symbols leaves the normaliser almost nothing to round.
const WEIGHT_ACCURACY: u32 = 6;

/// Write the weight description, choosing the cheaper of the two encodings.
///
/// **Direct** form: a header byte of `127 + count`, then weights four bits
/// each. Simple, and it tops out at 128 explicit weights — which sounds
/// generous and is not, because the count runs to the *largest symbol value*,
/// not the number of distinct symbols. Every corpus file here uses fewer than
/// a hundred distinct bytes and still reaches byte 194 or beyond, so the direct
/// form cannot describe any of them.
///
/// **FSE** form: a header byte holding the compressed size, then an FSE table
/// description and the weights coded against it. Needed for real data, for the
/// reason above, and usually smaller anyway.
pub fn write_weights(out: &mut Vec<u8>, code: &Code) -> bool {
    let weights = weight_sequence(code);
    if weights.is_empty() || weights.len() > 255 {
        return false;
    }

    if let Some(fse) = fse_weights(&weights) {
        // The header byte carries the compressed size and must stay below 128,
        // which is also what distinguishes this form from the direct one.
        if fse.len() < 128 && fse.len() + 1 < weights.len().div_ceil(2) + 1 {
            out.push(fse.len() as u8);
            out.extend_from_slice(&fse);
            return true;
        }
    }

    if code.max_symbol > 128 {
        // Direct form cannot express a header byte this large.
        return false;
    }
    out.push((127 + weights.len()) as u8);
    for pair in weights.chunks(2) {
        let low = pair.get(1).copied().unwrap_or(0);
        out.push((pair[0] << 4) | low);
    }
    true
}

/// FSE-compress a weight sequence, or `None` if it cannot be.
fn fse_weights(weights: &[u8]) -> Option<Vec<u8>> {
    // Two symbols is `FSE_compress`'s floor, and a run of one distinct weight
    // has no distribution to code against.
    if weights.len() <= 2 {
        return None;
    }
    let max = usize::from(*weights.iter().max()?);
    let mut counts = vec![0u32; max + 1];
    for &w in weights {
        counts[usize::from(w)] += 1;
    }
    if counts.iter().filter(|&&c| c > 0).count() < 2 {
        return None;
    }

    let norm = fse::normalize(&counts, WEIGHT_ACCURACY)?;
    // The description must end on a non-zero count; trailing zeros would leave
    // a run length owed and never written.
    let last = norm.iter().rposition(|&n| n != 0)?;
    let norm = &norm[..=last];

    let mut out = Vec::new();
    fse::write_table_description(&mut out, norm, WEIGHT_ACCURACY);
    let table = fse::CTable::build(norm, WEIGHT_ACCURACY);
    out.extend_from_slice(&fse::encode_interleaved(&table, weights)?);
    Some(out)
}

/// Encode one segment into its own backward bitstream.
///
/// Symbols are written last to first, because the reader consumes the stream
/// from its end — the same reversal the sequence bitstream needs, and for the
/// same reason.
fn encode_segment(code: &Code, segment: &[u8]) -> Vec<u8> {
    let mut w = BitWriter::new();
    for &symbol in segment.iter().rev() {
        let s = usize::from(symbol);
        w.add_bits(u64::from(code.values[s]), u32::from(code.lengths[s]));
    }
    w.close()
}

/// Encode literals into four streams with their jump table.
///
/// Four rather than one is not a choice: a single-stream literals section uses
/// `Size_Format 00`, whose sizes are ten bits, so it cannot describe more than
/// 1023 literals. Any real block needs the four-stream form, which exists so a
/// decoder can run the streams independently.
pub fn encode_four_streams(code: &Code, literals: &[u8]) -> Option<Vec<u8>> {
    let segment = literals.len().div_ceil(STREAMS);
    // `decode_four` computes the last segment as `regenerated - 3 * segment`
    // and refuses a negative result, so this must hold before anything is
    // written.
    if literals.len() < segment * (STREAMS - 1) {
        return None;
    }

    let mut parts: Vec<Vec<u8>> = Vec::with_capacity(STREAMS);
    for i in 0..STREAMS {
        let start = (i * segment).min(literals.len());
        let end = if i == STREAMS - 1 {
            literals.len()
        } else {
            ((i + 1) * segment).min(literals.len())
        };
        parts.push(encode_segment(code, &literals[start..end]));
    }

    // The jump table holds the first three sizes as 16-bit values, so a stream
    // larger than that cannot be described.
    if parts[..STREAMS - 1]
        .iter()
        .any(|p| p.len() > u16::MAX as usize)
    {
        return None;
    }

    let mut out = Vec::with_capacity(6 + parts.iter().map(Vec::len).sum::<usize>());
    for part in &parts[..STREAMS - 1] {
        out.extend_from_slice(&(part.len() as u16).to_le_bytes());
    }
    for part in &parts {
        out.extend_from_slice(part);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::huf;

    /// Every code this builds must round trip through the decoder's own table.
    ///
    /// The weight description and the canonical values are two independent
    /// chances to disagree with the decoder, and neither shows up as an error —
    /// a table off by one symbol decodes to plausible bytes. So the assertion
    /// is the whole path: weights out, table rebuilt by the decoder, literals
    /// encoded and decoded back.
    fn round_trip(literals: &[u8]) {
        let counts = counts(literals);
        let code = build(&counts).expect("two or more distinct symbols");

        let mut weights = Vec::new();
        assert!(
            write_weights(&mut weights, &code),
            "weights must be writable"
        );
        let table = huf::Table::parse(&weights)
            .expect("the decoder rebuilds it")
            .0;
        assert_eq!(
            table.table_log(),
            code.table_log,
            "the decoder derived a different tableLog"
        );

        let payload = encode_four_streams(&code, literals).expect("encodable");
        let mut out = Vec::new();
        table
            .decode_four(&payload, literals.len(), &mut out)
            .expect("decodes");
        assert_eq!(out, literals, "literals differ after a round trip");
    }

    #[test]
    fn text_round_trips() {
        round_trip(&b"the quick brown fox jumps over the lazy dog. ".repeat(40));
    }

    #[test]
    fn two_symbols_round_trip() {
        // The smallest alphabet a Huffman table can describe at all.
        let literals: Vec<u8> = (0..500u32)
            .map(|i| if i % 3 == 0 { b'a' } else { b'b' })
            .collect();
        round_trip(&literals);
    }

    #[test]
    fn a_skewed_alphabet_stays_within_the_length_limit() {
        // Frequencies falling off geometrically drive a naive Huffman tree far
        // past 11 bits, so this is the case the repair exists for.
        let mut literals = Vec::new();
        for symbol in 0..32u32 {
            let n = 1u32 << (31 - symbol).min(20);
            literals.extend(core::iter::repeat_n(
                symbol as u8,
                (n / 1024).max(1) as usize,
            ));
        }
        let counts = counts(&literals);
        let code = build(&counts).expect("buildable");
        assert!(
            code.table_log <= TABLELOG_MAX,
            "tableLog {} exceeds the format maximum",
            code.table_log
        );
        round_trip(&literals);
    }

    #[test]
    fn the_full_alphabet_round_trips() {
        // 256 symbols, far past what the direct weight form can describe, so
        // this only passes through the FSE description. Skewed rather than
        // uniform: see below for why uniform is a different case.
        let mut literals = Vec::new();
        for symbol in 0..=255u8 {
            literals.extend(core::iter::repeat_n(symbol, 1 + usize::from(symbol) % 7));
        }
        round_trip(&literals);
    }

    #[test]
    fn a_uniform_alphabet_falls_back_rather_than_failing() {
        // Every symbol equally frequent gives every symbol the same weight, so
        // the weight sequence has one distinct value and no distribution to
        // code against. There is also nothing to gain: eight bits a symbol is
        // what raw literals already cost. Refusing here is the right answer,
        // and the caller emits a raw section.
        let literals: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
        let code = build(&counts(&literals)).expect("buildable");
        let mut weights = Vec::new();
        assert!(
            !write_weights(&mut weights, &code),
            "a uniform alphabet has no weight distribution to describe"
        );
        assert!(weights.is_empty(), "nothing should have been written");
    }

    #[test]
    fn a_sparse_high_alphabet_round_trips() {
        // The shape every file in the corpus has: fewer than a hundred distinct
        // bytes, but a maximum well above 128. The direct form cannot express
        // it at all, which is why the FSE description had to be written.
        let mut literals = Vec::new();
        for (i, symbol) in [b'a', b'e', b't', 0xC3, 0xA9, 0xE2, 0x80, 0x99, 0xEF]
            .iter()
            .enumerate()
        {
            literals.extend(core::iter::repeat_n(*symbol, 200 >> i.min(6)));
        }
        assert!(*literals.iter().max().unwrap() > 128);
        round_trip(&literals);
    }

    #[test]
    fn a_single_symbol_is_refused() {
        // Its weights would sum to zero, which the decoder rejects. Such
        // literals belong in an RLE section.
        assert!(build(&counts(&[7u8; 100])).is_none());
        assert!(build(&counts(&[])).is_none());
    }

    #[test]
    fn huffman_beats_raw_on_text() {
        let literals = b"the quick brown fox jumps over the lazy dog. ".repeat(40);
        let counts = counts(&literals);
        let code = build(&counts).expect("buildable");
        let bits = code.encoded_bits(&counts);
        assert!(
            bits / 8 < literals.len() as u64,
            "{} bits for {} bytes is not a saving",
            bits,
            literals.len()
        );
    }
}
