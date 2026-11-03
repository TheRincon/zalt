//! Finding matches: hash chains, lazy evaluation, and history across blocks.
//!
//! ## What changed, and what deliberately did not
//!
//! The first version was one hash slot per position, greedy acceptance, and
//! matching confined to the current block. Entropy coding closed the gap to
//! `zstd -1` on small documents, which left the parse as the whole remaining
//! difference — so this lifts two of those three limits:
//!
//! - **Hash chains.** Each hash keeps a chain of earlier positions rather than
//!   only the most recent, and the search takes the longest match among the
//!   first [`search_depth`](Tuning::search_depth). One slot means the *newest*
//!   candidate wins, which is frequently not the longest.
//! - **Lazy matching.** A match found at `pos` is not taken until the position
//!   after it has been checked; if `pos + 1` starts a longer match, the byte at
//!   `pos` becomes a literal instead. Greedy parsing takes the first match it
//!   sees and can strand a better one a byte later.
//! - **History across blocks.** Offsets may reach back through the declared
//!   window rather than stopping at the block boundary, so the second and later
//!   blocks of a frame can match into the first.
//!
//! **Repeat offsets are still never emitted.** `Offset_Value` 1-3 select the
//! three recent offsets, and §4 records the `literals_length == 0` case in
//! their update as "the single most commonly botched part of the format".
//! Every offset here is a literal `offset + 3`, so the encoder holds no repeat
//! state to get wrong. That costs bits on structured data and is the next thing
//! to try — deliberately after these, which cannot produce an illegal sequence
//! at all.

use alloc::vec;
use alloc::vec::Vec;

use core::marker::PhantomData;

use super::level::{Finder, Parse as ParseMode, Tuning};
use super::optimal::{Prices, Stats};
use crate::backend::{Backend, Scalar};
use crate::sequences::{REP_COUNT, REP_INITIAL, resolve_offset};

/// Shortest match worth emitting.
///
/// The format's floor is 3, from match-length code 0. Four is used because the
/// hash reads four bytes anyway, and a three-byte match rarely pays for the
/// sequence describing it.
const MIN_MATCH: usize = 4;

/// How far the binary tree compares two positions before giving up on
/// ordering them.
///
/// The tree orders positions by the bytes that follow them; two that agree all
/// the way to this limit cannot be ordered, and the tree forgets one of them
/// and everything below it. That is the price of a bound, and it was measured:
/// at 4 KiB — libzstd's `ZSTD_OPT_NUM` — level 19 came out 0.7% larger on large
/// input than at 64 KiB, because text with long duplicated stretches made the
/// tree forget constantly. The bound exists at all because the cost of
/// inserting a long duplicated region grows with the square of its length;
/// [`Matcher::insert_up_to`]'s skip is what keeps that affordable at 64 KiB.
const TREE_COMPARE_MAX: usize = 1 << 16;

/// A tree insertion that found a match longer than this lets the next few
/// positions go uninserted — libzstd's `ZSTD_insertBt1` speed rule, with its
/// numbers. Positions deep inside a long match are the ones least likely to be
/// the best source for anything later, and inserting each costs a compare as
/// long as the match.
const TREE_SKIP_AFTER: usize = 384;

/// The most positions one such insertion skips.
const TREE_SKIP_MAX: usize = 192;

/// Bytes at the end of a block left as literals rather than matched into.
///
/// The hash reads four bytes at a time, so the last few positions have nothing
/// to hash. It also guarantees a block ends with a literal run, which keeps the
/// sequence section's trailing literals non-degenerate.
const TAIL: usize = 8;

/// Candidates examined per position before settling, for a matcher built
/// without a level — see [`Matcher::new`]. A level sets its own.
///
/// Cost is linear in this and the benefit falls off quickly.
#[cfg(test)]
const SEARCH_DEPTH: usize = 32;

/// A later match must beat the one in hand by this much to be worth the literal
/// that taking it costs.
const LAZY_MARGIN: usize = 1;

/// The fast finders' step: after a miss, the next position searched is one
/// further on for every `2^SEARCH_STRENGTH` bytes since the last match —
/// libzstd's `kSearchStrength`. Incompressible stretches are crossed quickly;
/// the first match ends the acceleration.
const SEARCH_STRENGTH: u32 = 8;

/// Largest hash table the matcher will build, as a log2.
///
/// Also the *only* size it used to build. A 2^17 table of `u32` is 512 KiB, and
/// with a chain of the same order that is around a megabyte zeroed per frame —
/// which §8's workload pays once per resource, on resources of a few kilobytes.
/// The M5 profile found `memset` at 7.6% of a 7.7 KB encode, all of it clearing
/// table space for positions that do not exist.
#[cfg(test)]
const HASH_LOG_MAX: u32 = 17;

/// Smallest hash table worth building.
///
/// Below a few thousand slots the table stops separating anything and the chain
/// walk does all the work, spending its [`search_depth`](Tuning::search_depth)
/// budget on collisions instead of candidates.
const HASH_LOG_MIN: u32 = 12;

/// Hash bits for an input of this size.
///
/// One slot per input position, clamped. A 4 KB resource cannot fill more than
/// 4096 slots however large the table is, so sizing to the content is not a
/// tuning compromise on small inputs — it is declining to clear memory that
/// provably stays empty. Inputs at or above 128 KB get the full table and are
/// unaffected.
fn hash_log_for(len: usize, max: u32) -> u32 {
    let needed = usize::BITS - len.max(1).leading_zeros();
    needed.clamp(HASH_LOG_MIN, max.max(HASH_LOG_MIN))
}

/// Marks an empty slot. Unreachable as a position: a frame this encoder emits
/// is bounded well below 4 GiB.
const EMPTY: u32 = u32::MAX;

/// The deferred tree's mark, in a position's second slot, for "in its bucket
/// but not yet sorted". libzstd uses 1, which a real position can equal; this
/// cannot, for the same reason [`EMPTY`] cannot. And a descent that read it
/// as a child would stop there: the tree's age check rejects any child not
/// older than its parent, and nothing is older than this.
const UNSORTED: u32 = u32::MAX - 1;

/// One LZ77 sequence: literals, then a match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sequence {
    /// Literals preceding the match.
    pub literal_length: u32,
    /// Bytes copied from `offset` back.
    pub match_length: u32,
    /// Distance backwards to the match source. Always at least 1.
    pub offset: u32,
    /// The `Offset_Value` that encodes it: 1-3 name a recent offset, anything
    /// larger is `offset + 3`.
    ///
    /// Chosen here rather than in the section writer because the *search* wants
    /// to know which offsets are cheap. A repeat costs a code of 0 or 1 where a
    /// literal 128 KiB offset costs seventeen bits, so a match one byte shorter
    /// at a repeat offset is very often the better sequence — and only the
    /// matcher is in a position to weigh that.
    pub offset_value: u32,
}

/// Dictionary history in front of a frame, as the matcher sees it.
///
/// The encoder puts the dictionary's content in the same buffer as the frame,
/// first, so a match into it is an ordinary backwards offset — the very
/// arithmetic the decoder does, with the dictionary as history before the
/// frame's first byte (RFC 8878 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prefix {
    /// Bytes of dictionary content at the start of the buffer.
    pub len: usize,
    /// The repeat table the frame starts from: a formatted dictionary's own
    /// three offsets, which the decoder loads in place of `{1, 4, 8}`.
    pub repeats: [u32; REP_COUNT],
}

impl Prefix {
    /// No dictionary.
    pub const NONE: Self = Self {
        len: 0,
        repeats: REP_INITIAL,
    };
}

/// What one block decomposed into.
pub struct Parse {
    pub sequences: Vec<Sequence>,
    /// Every literal in the block, in order, including the trailing run.
    pub literals: Vec<u8>,
}

fn hash4(bytes: &[u8], hash_log: u32) -> usize {
    let v = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    // Knuth's multiplicative constant; the top bits of the product are the best
    // mixed, so the shift takes those.
    (v.wrapping_mul(2_654_435_761) >> (32 - hash_log)) as usize
}

/// libzstd's multiplicative hashes of the low `n` bytes of a little-endian
/// word (`zstd_compress_internal.h`, 1.5.7). The shift left discards the bytes
/// past `n` before multiplying, so only those `n` bytes reach the top bits the
/// shift right keeps.
fn hash_n(word: u64, n: u8, log: u32) -> usize {
    const PRIME5: u64 = 889_523_592_379;
    const PRIME6: u64 = 227_718_039_650_203;
    const PRIME7: u64 = 58_295_818_150_454_627;
    const PRIME8: u64 = 0xCF1B_BCDC_B7A5_6463;
    let product = match n {
        5 => (word << 24).wrapping_mul(PRIME5),
        6 => (word << 16).wrapping_mul(PRIME6),
        7 => (word << 8).wrapping_mul(PRIME7),
        _ => word.wrapping_mul(PRIME8),
    };
    (product >> (64 - log)) as usize
}

/// Which bytes of `row` equal `tag`: bit `i` of the result for byte `i`. A
/// row is 16, 32 or 64 bytes.
///
/// The row finder's one data-parallel step, kept to this function so a NEON
/// version can take its place from `zalt-neon` (the crate allowed `unsafe`).
/// This one is SWAR in safe Rust: per eight bytes, XOR with the tag repeated
/// makes matching bytes zero; the exact zero-byte test below flags them in
/// each byte's top bit (no false positives, unlike the cheaper `x − 0x01…`
/// form), and a multiply gathers the eight flags into one byte.
fn tag_matches(row: &[u8], tag: u8) -> u64 {
    const LOW7: u64 = 0x7F7F_7F7F_7F7F_7F7F;
    const HIGH: u64 = 0x8080_8080_8080_8080;
    // Bit 8k (one per byte) to bit 56 + k: byte k's flag times 2^(56 − 7k).
    // No two terms land on one bit, so nothing carries into the top byte.
    const GATHER: u64 = 0x0102_0408_1020_4080;
    let pattern = u64::from(tag) * 0x0101_0101_0101_0101;
    let mut mask = 0u64;
    for (i, chunk) in row.as_chunks::<8>().0.iter().enumerate() {
        let x = u64::from_le_bytes(*chunk) ^ pattern;
        let zero = !(((x & LOW7) + LOW7) | x) & HIGH;
        mask |= ((zero >> 7).wrapping_mul(GATHER) >> 56) << (8 * i);
    }
    mask
}

/// Bytes the row finder hashes — libzstd's `BOUNDED(4, minMatch, 6)` for
/// its levels 5–12, which all set 5.
const ROW_HASH_BYTES: u8 = 5;

/// Eight bytes at `pos`, or `None` within eight of the end — the fast
/// finders hash and compare whole words.
fn word_at(input: &[u8], pos: usize) -> Option<u64> {
    input
        .get(pos..)?
        .first_chunk::<8>()
        .map(|w| u64::from_le_bytes(*w))
}

fn four_at(input: &[u8], pos: usize) -> Option<u32> {
    input
        .get(pos..)?
        .first_chunk::<4>()
        .map(|w| u32::from_le_bytes(*w))
}

/// The `Offset_Value` that encodes `offset`, and the repeat table after it.
///
/// **The choice is made by asking the decoder.** §4 records the
/// `literals_length == 0` shift as the most commonly botched part of the format,
/// and it is not a rule worth re-deriving: each candidate is tried against
/// [`resolve_offset`] on a copy of the table, and whichever reproduces the
/// offset wins. Pure, so the optimal parser can price a repeat code along a
/// path it has not committed to, with the same rule that later writes it.
fn offset_code_for(
    repeats: [u32; REP_COUNT],
    offset: u32,
    literals_zero: bool,
) -> (u32, [u32; REP_COUNT]) {
    // Most offsets are new, and a new offset can only be written one way. The
    // repeat codes resolve to `repeats[0..3]`, or — the `literals_length == 0`
    // shift — to `repeats[1]`, `repeats[2]` and `repeats[0] - 1`; an offset
    // that is none of those is a literal offset, and the decoder's table after
    // it is the old one shifted down. Trying all three repeat codes first made
    // this 8% of encode time at level 1. The decoder still has the last word,
    // in every debug build.
    let named = offset == repeats[0]
        || offset == repeats[1]
        || offset == repeats[2]
        || (literals_zero && offset == repeats[0].wrapping_sub(1));
    if !named {
        let value = offset + REP_COUNT as u32;
        let after = [offset, repeats[0], repeats[1]];
        debug_assert!({
            let mut trial = repeats;
            resolve_offset(value, literals_zero, &mut trial) == Ok(offset) && trial == after
        });
        return (value, after);
    }
    for candidate in 1..=REP_COUNT as u32 {
        let mut trial = repeats;
        if resolve_offset(candidate, literals_zero, &mut trial) == Ok(offset) {
            return (candidate, trial);
        }
    }
    let value = offset + REP_COUNT as u32;
    let mut trial = repeats;
    let resolved = resolve_offset(value, literals_zero, &mut trial);
    debug_assert_eq!(resolved, Ok(offset));
    (value, trial)
}

/// Positions the optimal parser explores before committing to a path — libzstd's
/// `ZSTD_OPT_NUM`. Bounds the table, and the distance a decision can be
/// deferred.
const OPT_NUM: usize = 1 << 12;

/// One position in the optimal parser's table: the cheapest known way to reach
/// it, and the state that way leaves behind.
#[derive(Debug, Clone, Copy)]
struct Node {
    /// Cost of the cheapest path here, in [`super::optimal::BIT`] units.
    /// Signed: a literal step charges the *change* in the literals-length
    /// price, which can be negative when a longer run has a commoner code.
    price: i64,
    /// Length of the match that ends here on that path, or 0 for a literal.
    match_length: u32,
    offset: u32,
    /// Literals pending since the path's last match.
    literals: u32,
    /// The decoder's repeat table, had it followed this path. Repeat matches
    /// are priced from it, so two paths reaching one position can disagree
    /// about which offsets are cheap — and should.
    repeats: [u32; REP_COUNT],
}

const UNREACHED: Node = Node {
    price: i64::MAX,
    match_length: 0,
    offset: 0,
    literals: 0,
    repeats: [0; REP_COUNT],
};

fn ensure(opt: &mut Vec<Node>, index: usize) {
    if opt.len() <= index {
        opt.resize(index + 1, UNREACHED);
    }
}

/// Offer every match length of every candidate from `from` to the table.
///
/// Candidates arrive in increasing length, each at a farther (dearer) offset
/// than the last. So a length is best served by the *first* candidate long
/// enough to reach it: candidate `k` prices only the lengths candidate `k - 1`
/// could not.
fn relax(
    opt: &mut Vec<Node>,
    from: usize,
    candidates: &[(u32, u32)],
    prices: &Prices,
    last: &mut usize,
) {
    let node = opt[from];
    let literals_zero = node.literals == 0;
    // The sequence's literals-length price for a run of zero; a longer run has
    // already paid the difference, one literal step at a time.
    let base = node.price + i64::from(prices.literal_length(0));
    let mut shortest = MIN_MATCH as u32;
    for &(length, offset) in candidates {
        let (value, repeats) = offset_code_for(node.repeats, offset, literals_zero);
        let reach = from + length as usize;
        ensure(opt, reach);
        for ml in shortest..=length {
            let price = base + i64::from(prices.sequence(ml, value));
            let slot = &mut opt[from + ml as usize];
            if price < slot.price {
                *slot = Node {
                    price,
                    match_length: ml,
                    offset,
                    literals: 0,
                    repeats,
                };
            }
        }
        shortest = length + 1;
        *last = (*last).max(reach);
    }
}

/// A match finder whose history persists across the blocks of one frame.
///
/// Generic over the [`Backend`] that measures matches — `common_prefix`, the
/// one encoder kernel. [`Scalar`] unless a caller picks another.
pub struct Matcher<B = Scalar> {
    /// Most recent position for each hash. For [`Finder::DoubleFast`], the
    /// short table.
    head: Vec<u32>,
    /// [`Finder::DoubleFast`]'s long table, keyed on eight bytes. Empty
    /// otherwise.
    long: Vec<u32>,
    long_log: u32,
    /// [`Finder::Row`]'s tags, one byte per slot of `head`; each row's first
    /// byte is instead the row's head index. Empty otherwise.
    tags: Vec<u8>,
    /// `log2` of the row length, for [`Finder::Row`]; 0 otherwise.
    row_log: u32,
    /// Bits of hash `head` is indexed by. Sized to the input — see
    /// [`hash_log_for`].
    hash_log: u32,
    /// Previous position sharing a hash, indexed by position modulo the window.
    chain: Vec<u32>,
    chain_mask: usize,
    /// How candidates are found. See [`Finder`].
    finder: Finder,
    /// The binary tree, for [`Finder::Tree`]: two child slots per position —
    /// the earlier position that sorts just below it, and the one just above —
    /// indexed by position modulo the tree's size. Empty for the chain.
    tree: Vec<u32>,
    tree_mask: usize,
    /// Whether the tree defers sorting — libzstd's `btlazy2` — which it does
    /// under the greedy and lazy parses. See [`Matcher::find_tree`].
    deferred: bool,
    /// Scratch for the deferred tree: the unsorted positions a search walked.
    unsorted: Vec<u32>,
    /// Largest offset the frame's declared window admits.
    max_offset: usize,
    /// Bytes of dictionary in front of the frame: `input[..dict_len]` is
    /// history the decoder holds before the frame begins. Zero without one.
    dict_len: usize,
    /// Last position from which a match may still reach into the dictionary:
    /// `dict_len + max_offset`. See [`Matcher::reach`].
    reach_end: usize,
    /// The repeat table the frame starts from — the dictionary's, or the
    /// format's `{1, 4, 8}` — kept so a scout can start where this did.
    first_repeats: [u32; REP_COUNT],
    /// Chain candidates examined per search.
    depth: usize,
    /// How the candidates found are chosen among.
    mode: ParseMode,
    /// A match this long ends the search early. See [`Tuning::target_length`].
    target_length: usize,
    /// The level's knobs and the declared window, kept so a two-pass level can
    /// build a scout matcher identical to this one.
    tuning: Tuning,
    window_log: u8,
    /// First position not yet inserted. The optimal parser uses it with either
    /// finder — it searches positions out of the order the lazy loop visits
    /// them, and catches up to wherever it is about to search — and the lazy
    /// loop uses it with a tree, where searching inserts.
    indexed: usize,
    /// What the optimal parser prices from, carried across blocks.
    stats: Option<Stats>,
    /// Whether a two-pass level has already scouted this frame's first block.
    scouted: bool,
    /// Scratch for the optimal parser, kept so it is allocated once per frame
    /// rather than once per stretch of input.
    opt: Vec<Node>,
    candidates: Vec<(u32, u32)>,
    path: Vec<(usize, usize, usize)>,
    /// The three recent offsets, as the decoder will hold them.
    ///
    /// Advanced through [`crate::sequences::resolve_offset`] so it cannot drift
    /// from the decoder's copy. Reset per *frame*, which is why it lives with
    /// the matcher rather than being rebuilt per block.
    repeats: [u32; REP_COUNT],
    /// The kernel that measures matches. Zero-sized.
    backend: PhantomData<B>,
}

impl<B: Backend> Matcher<B> {
    /// A matcher for a frame with this `windowLog`, encoding `input_len` bytes.
    ///
    /// **`input_len` sizes the tables and nothing else.** It is not a limit —
    /// every offset is still bounded by the window, and a caller that passes a
    /// larger figure gets a larger table and the same output.
    ///
    /// This is the configuration the encoder had before levels existed — depth
    /// 32, one lazy step, a hash table of at most 2^17 — and it is kept, for the
    /// tests, as a fixed point: a level that sets the same knobs must parse the
    /// same input to the same sequences.
    #[cfg(test)]
    pub fn new(window_log: u8, input_len: usize) -> Self {
        Self::with_tuning(
            Tuning {
                window_log,
                hash_log: HASH_LOG_MAX,
                search_depth: SEARCH_DEPTH,
                parse: ParseMode::Lazy(1),
                finder: Finder::Chain,
                target_length: usize::MAX,
            },
            window_log,
            input_len,
        )
    }

    /// A matcher for one compression level.
    ///
    /// `window_log` is what the frame actually declares, which the caller has
    /// already capped — by its own ceiling and by the content's size — so it may
    /// be smaller than the level's. The level's own window is not consulted here
    /// for exactly that reason: an offset past the declared window names history
    /// the decoder has been told it may drop.
    #[cfg(test)]
    pub fn with_tuning(tuning: Tuning, window_log: u8, input_len: usize) -> Self {
        Self::with_prefix(tuning, window_log, input_len, Prefix::NONE)
    }

    /// A matcher for a frame encoded after `prefix.len` bytes of dictionary.
    ///
    /// `input_len` counts the dictionary too: it is the length of the buffer
    /// [`Matcher::parse`] will see, `input[..prefix.len]` being the
    /// dictionary. [`Matcher::prime`] must index it before the first parse.
    pub fn with_prefix(tuning: Tuning, window_log: u8, input_len: usize, prefix: Prefix) -> Self {
        let window = 1usize << window_log.min(31);
        // The chain is indexed modulo its own size, so entries older than the
        // window are overwritten and drop out — which is exactly the set of
        // positions an offset could no longer reach anyway. A dictionary
        // reaches further, a window *plus* the dictionary (see `reach`), so
        // the table covers both.
        //
        // Capping it at the input length as well is free of consequence rather
        // than a trade: a chain at least as long as the input never has two
        // live positions sharing a slot, so nothing that could have been found
        // in the larger table is missing from this one. It only declines to
        // clear slots no position can reach.
        let chain_size = (window + prefix.len).next_power_of_two().min(
            input_len
                .max(1)
                .checked_next_power_of_two()
                .unwrap_or(window),
        );
        // The fast finders have their own greedy loops and nothing else, and
        // rows serve only the greedy/lazy loop. A tree serves both parses: in a
        // tree searching *is* inserting, and both keep it in order through the
        // `indexed` cursor — see `find_tree`.
        let finder = match (tuning.finder, tuning.parse) {
            (Finder::Tree, _) => Finder::Tree,
            (f @ (Finder::Fast { .. } | Finder::DoubleFast { .. }), ParseMode::Greedy) => f,
            (f @ Finder::Row { .. }, ParseMode::Greedy | ParseMode::Lazy(_)) => f,
            _ => Finder::Chain,
        };
        let (hash_log, long_log) = match finder {
            Finder::DoubleFast { short_log, .. } => (
                hash_log_for(input_len, short_log),
                hash_log_for(input_len, tuning.hash_log),
            ),
            _ => (hash_log_for(input_len, tuning.hash_log), 0),
        };
        // Whichever structure is not in use is a single slot rather than a
        // window's worth, so a tree level does not also pay to clear a chain.
        let (chain_size, tree_size) = match finder {
            Finder::Chain => (chain_size, 0),
            Finder::Tree => (1, chain_size),
            Finder::Fast { .. } | Finder::DoubleFast { .. } | Finder::Row { .. } => (1, 0),
        };
        let row_log = match finder {
            Finder::Row { row_log } => u32::from(row_log),
            _ => 0,
        };
        Self {
            head: vec![EMPTY; 1 << hash_log],
            long: if long_log > 0 {
                vec![EMPTY; 1 << long_log]
            } else {
                Vec::new()
            },
            long_log,
            tags: if row_log > 0 {
                vec![0; 1 << hash_log]
            } else {
                Vec::new()
            },
            row_log,
            hash_log,
            chain: vec![EMPTY; chain_size],
            chain_mask: chain_size - 1,
            finder,
            tree: vec![EMPTY; 2 * tree_size],
            tree_mask: tree_size.saturating_sub(1),
            deferred: finder == Finder::Tree && !matches!(tuning.parse, ParseMode::Optimal { .. }),
            unsorted: Vec::new(),
            max_offset: window,
            dict_len: prefix.len,
            reach_end: prefix.len + window,
            first_repeats: prefix.repeats,
            depth: tuning.search_depth.max(1),
            mode: tuning.parse,
            target_length: tuning.target_length.max(MIN_MATCH),
            tuning,
            window_log,
            indexed: 0,
            stats: None,
            scouted: false,
            opt: Vec::new(),
            candidates: Vec::new(),
            path: Vec::new(),
            repeats: prefix.repeats,
            backend: PhantomData,
        }
    }

    /// The largest offset a match starting at `pos` may name.
    ///
    /// RFC 8878 §5: a frame may reach into its dictionary — even further back
    /// than `Window_Size` — "as long as the amount of data decoded from this
    /// frame is less than or equal to `Window_Size`", and not at all after.
    /// The decoder applies that at each match's start (`Window::dict_reach`),
    /// so this does too: until the frame has produced a window's worth, a
    /// match may reach back to the dictionary's first byte, which is position
    /// 0 here; after it, only the window.
    ///
    /// With no dictionary this is `min(pos, window)` — the bound every finder
    /// applied before dictionaries existed, as `c < pos` and `pos - c <=
    /// window` — so frames without one are unchanged.
    #[inline]
    fn reach(&self, pos: usize) -> usize {
        if pos <= self.reach_end {
            pos
        } else {
            self.max_offset
        }
    }

    /// Index the dictionary, `input[..dict_len]`, so the frame's first block
    /// can match into it. Call once, before the first [`Matcher::parse`].
    ///
    /// Every position goes in, for every finder, as libzstd's dictionary
    /// loading fills its tables: the dictionary is indexed once per frame
    /// rather than searched, so the fast finders' skipping, which saves
    /// searches, saves nothing here.
    pub fn prime(&mut self, input: &[u8]) {
        let d = self.dict_len;
        match self.finder {
            Finder::Fast { min_match } => {
                for pos in 0..d {
                    if let Some(w) = word_at(input, pos) {
                        self.head[hash_n(w, min_match, self.hash_log)] = pos as u32;
                    }
                }
            }
            Finder::DoubleFast { min_match, .. } => {
                for pos in 0..d {
                    if let Some(w) = word_at(input, pos) {
                        self.long[hash_n(w, 8, self.long_log)] = pos as u32;
                        self.head[hash_n(w, min_match, self.hash_log)] = pos as u32;
                    }
                }
            }
            Finder::Row { .. } => {
                self.index_range(input, 0, d);
                self.indexed = d;
            }
            // Through the cursor, which the tree needs in order and the
            // optimal parser continues from.
            Finder::Chain | Finder::Tree => self.insert_up_to(input, d),
        }
    }

    fn insert(&mut self, input: &[u8], pos: usize) {
        if self.row_log > 0 {
            return self.insert_row(input, pos);
        }
        if self.finder == Finder::Tree {
            // A position the lazy loop searched is already in; the cursor
            // knows, and inserting it again would corrupt the tree.
            return self.insert_up_to(input, pos + 1);
        }
        let h = hash4(&input[pos..], self.hash_log);
        self.chain[pos & self.chain_mask] = self.head[h];
        self.head[h] = pos as u32;
    }

    /// The row a position hashes to, as the index of its first slot, and its
    /// tag — or `None` within eight bytes of the end, where it cannot be
    /// hashed.
    fn row_of(&self, input: &[u8], pos: usize) -> Option<(usize, u8)> {
        let hash = hash_n(word_at(input, pos)?, ROW_HASH_BYTES, self.hash_log + 8);
        let row = ((hash >> 8) << self.row_log) & (self.head.len() - 1);
        Some((row, hash as u8))
    }

    /// Put `pos` at the head of its row, over the oldest entry. Slot 0 of each
    /// row holds the head index rather than a position, as in libzstd, so a
    /// row of `2^row_log` keeps `2^row_log − 1` positions.
    fn insert_row(&mut self, input: &[u8], pos: usize) {
        let Some((row, tag)) = self.row_of(input, pos) else {
            return;
        };
        let mask = (1usize << self.row_log) - 1;
        let mut next = usize::from(self.tags[row]).wrapping_sub(1) & mask;
        if next == 0 {
            next = mask;
        }
        self.tags[row] = next as u8;
        self.tags[row + next] = tag;
        self.head[row + next] = pos as u32;
    }

    /// The row finder's search: the candidates in `pos`'s row whose tag
    /// matches, newest first, up to `depth` of them, compared in full.
    fn find_row(&self, input: &[u8], pos: usize, limit: usize) -> Option<(usize, usize)> {
        let available = limit.saturating_sub(pos);
        if available < MIN_MATCH {
            return None;
        }
        let (row, tag) = self.row_of(input, pos)?;
        let entries = 1usize << self.row_log;
        let mask = entries - 1;
        let head = usize::from(self.tags[row]) & mask;
        // Slot 0 is the head index, never a position.
        let matches = tag_matches(&self.tags[row..row + entries], tag) & !1;
        // Rotate so bit 0 is the head: ascending bits are then newest first.
        let all = if entries == 64 {
            u64::MAX
        } else {
            (1u64 << entries) - 1
        };
        let mut order = if head == 0 {
            matches
        } else {
            ((matches >> head) | (matches << (entries - head))) & all
        };

        let reach = self.reach(pos);
        let mut best_len = 0usize;
        let mut best_offset = 0usize;
        let mut attempts = self.depth;
        while order != 0 && attempts > 0 {
            let slot = (head + order.trailing_zeros() as usize) & mask;
            order &= order - 1;
            let candidate = self.head[row + slot];
            let c = candidate as usize;
            if candidate == EMPTY || c >= pos {
                continue;
            }
            // Newest first, so every later candidate is further back still.
            if pos - c > reach {
                break;
            }
            attempts -= 1;
            // As in `find`: only a candidate that agrees where the best
            // match ends can be longer than it.
            if best_len > 0 && input[c + best_len] != input[pos + best_len] {
                continue;
            }
            let length = B::common_prefix(&input[c..], &input[pos..pos + available]);
            if length > best_len {
                best_len = length;
                best_offset = pos - c;
                if best_len == available || best_len >= self.target_length {
                    break;
                }
            }
        }
        (best_len >= MIN_MATCH).then_some((best_len, best_offset))
    }

    /// The lazy loop's search, whichever finder: `find`, or `find_tree`.
    fn search(&mut self, input: &[u8], pos: usize, limit: usize) -> Option<(usize, usize)> {
        if self.finder == Finder::Tree {
            self.find_tree(input, pos, limit)
        } else {
            self.find(input, pos, limit)
        }
    }

    /// Longest match at `pos` in the deferred tree, inserting `pos` —
    /// libzstd's `btlazy2` (`ZSTD_DUBT_findBestMatch`, 1.5.7).
    ///
    /// **Why deferred.** A tree that sorts each position as it is inserted
    /// pays a descent for every byte, match interiors included, and under the
    /// lazy parse — which searches only a fraction of positions — that cost
    /// is most of the work: measured 2026-10-11, it lost to plain chains at
    /// levels 13–14. So positions the parse does not search join their bucket
    /// *unsorted*, as a chain link ([`Matcher::add_unsorted`]), and are sorted
    /// only when a search here walks past them. Interiors no search reaches
    /// are never sorted at all.
    ///
    /// **The search** walks the bucket's unsorted positions newest first, up
    /// to the depth; cuts the list there if it is still going, dropping what
    /// is older — libzstd's trade, a little ratio for bounded time; sorts the
    /// walked positions into the tree oldest first, each becoming the root
    /// over everything older; and then searches the tree and makes `pos` its
    /// root, as the optimal parser's tree does.
    ///
    /// **Order.** The tree needs each position inserted exactly once and in
    /// order, and the lazy loop searches some positions and indexes the rest
    /// afterwards, so this goes through the cursor the optimal parser uses:
    /// everything before `pos` is caught up first, and the loop's later
    /// `insert` of `pos` finds the cursor already past it. A position the
    /// cursor has passed is not searched at all; it would be inserted twice.
    fn find_tree(&mut self, input: &[u8], pos: usize, limit: usize) -> Option<(usize, usize)> {
        let available = limit.saturating_sub(pos);
        if available < MIN_MATCH {
            return None;
        }
        self.insert_up_to(input, pos);
        if self.indexed != pos {
            return None;
        }

        // Walk the unsorted positions. Only above `reused_below`, where a
        // slot still belongs to the position that names it, and each strictly
        // older than the last — anything else is stale.
        let reused_below = pos.saturating_sub(self.tree_mask);
        let floor = pos - self.reach(pos);
        let mut walked = core::mem::take(&mut self.unsorted);
        walked.clear();
        let mut budget = self.depth;
        let mut node = self.head[hash4(&input[pos..], self.hash_log)];
        let mut previous = pos;
        let mask = self.tree_mask;
        let is_unsorted = |tree: &[u32], m: u32, previous: usize| {
            let m = m as usize;
            m < previous && m > reused_below && m >= floor && tree[2 * (m & mask) + 1] == UNSORTED
        };
        while node != EMPTY && is_unsorted(&self.tree, node, previous) && budget > 1 {
            walked.push(node);
            previous = node as usize;
            node = self.tree[2 * (node as usize & mask)];
            budget -= 1;
        }
        // Out of budget with the list still going: cut it here. The position
        // becomes a sorted leaf, and everything older drops out of the tree.
        if node != EMPTY && is_unsorted(&self.tree, node, previous) {
            let slots = 2 * (node as usize & self.tree_mask);
            self.tree[slots] = EMPTY;
            self.tree[slots + 1] = EMPTY;
        }
        // Oldest first, so each sorts into a tree of positions already
        // sorted. Its first slot is its link to the next older — the root of
        // that tree — and the descent overwrites both slots.
        for &u in walked.iter().rev() {
            let u = u as usize;
            let root = self.tree[2 * (u & self.tree_mask)];
            self.tree_descend(input, u, root, pos, budget, 0, 0, None);
            budget += 1;
        }
        self.unsorted = walked;

        let mut out = core::mem::take(&mut self.candidates);
        out.clear();
        self.tree_update(input, pos, available, MIN_MATCH - 1, Some(&mut out));
        self.indexed = pos + 1;
        // Reported shortest first, each longer than the last.
        let best = out
            .last()
            .map(|&(length, offset)| (length as usize, offset as usize));
        self.candidates = out;
        best
    }

    /// Put `pos` at the head of its bucket in the deferred tree, unsorted:
    /// its first slot links to the previous head, as a chain would, and its
    /// second holds [`UNSORTED`] until a search sorts it. libzstd's
    /// `ZSTD_updateDUBT`.
    fn add_unsorted(&mut self, input: &[u8], pos: usize) {
        let h = hash4(&input[pos..], self.hash_log);
        let slots = 2 * (pos & self.tree_mask);
        self.tree[slots] = self.head[h];
        self.tree[slots + 1] = UNSORTED;
        self.head[h] = pos as u32;
    }

    /// Longest match at `pos`, searching back through the chain.
    ///
    /// Returns `(length, offset)`. `limit` is the position matching may not run
    /// past — the block's tail boundary.
    fn find(&self, input: &[u8], pos: usize, limit: usize) -> Option<(usize, usize)> {
        if self.row_log > 0 {
            return self.find_row(input, pos, limit);
        }
        let available = limit.saturating_sub(pos);
        if available < MIN_MATCH {
            return None;
        }
        let floor = pos - self.reach(pos);

        let mut best_len = 0usize;
        let mut best_offset = 0usize;
        // The byte one past the end of the best match — what a candidate has to
        // agree with to be worth comparing. Held rather than re-loaded from
        // `input` for every candidate. Never read while `best_len` is zero.
        let mut best_tail = 0u8;
        let mut candidate = self.head[hash4(&input[pos..], self.hash_log)];

        for _ in 0..self.depth {
            if candidate == EMPTY {
                break;
            }
            let c = candidate as usize;
            if c < floor || c >= pos {
                break;
            }

            // **The next chain link is loaded before it is needed.**
            //
            // `candidate = chain[candidate]` is a pointer chase: each link
            // depends on the one before it, so the walk cannot be prefetched —
            // the address is not known until the previous load lands. Issuing
            // it at the top, where everything below is independent of it, lets
            // the latency overlap the comparison work rather than queue behind
            // it.
            let next = self.chain[c & self.chain_mask];

            // Reject on one byte before comparing any. A candidate is only
            // worth having if it is *longer* than the one in hand, so it must
            // at minimum agree at the byte where the best match ends. Most
            // candidates fail it — they share a four-byte hash and little else
            // — and the effect compounds: the longer the best match gets, the
            // cheaper each remaining candidate is to dismiss.
            //
            // Selection is unchanged by this, which is what makes it free
            // rather than a trade. A candidate agreeing up to `best_len` and
            // then differing *ties* the match in hand, and a tie never passed
            // `length > best_len` either.
            //
            // In range because `best_len < available` whenever it is non-zero
            // (the loop breaks on equality), so `c + best_len < pos + available
            // = limit <= input.len()`.
            if best_len == 0 || input[c + best_len] == best_tail {
                // The source runs to the end of the input, not to `pos`.
                // Bounding it at `pos` would cap every match at its own offset
                // and forbid overlapping matches — the offset-1 run that is
                // §5's win #2 has a one-byte source. Reaching past `pos` is
                // sound because the decoder copies byte by byte and reads back
                // what it has just written.
                let length = B::common_prefix(&input[c..], &input[pos..pos + available]);
                if length > best_len {
                    best_len = length;
                    best_offset = pos - c;
                    if best_len == available || best_len >= self.target_length {
                        // Nothing can beat a match that runs to the limit, and
                        // a level may declare a shorter one good enough.
                        break;
                    }
                    best_tail = input[pos + best_len];
                }
            }

            // The chain must strictly descend. Anything else is a stale entry
            // from a wrapped slot, and following it would loop forever.
            if next >= candidate {
                break;
            }
            candidate = next;
        }

        (best_len >= MIN_MATCH).then_some((best_len, best_offset))
    }

    /// The repeat table, for a caller that must roll it back.
    pub fn repeats(&self) -> [u32; REP_COUNT] {
        self.repeats
    }

    /// Restore a previously taken snapshot.
    ///
    /// Needed because a block whose compressed form does not pay is emitted
    /// stored instead, and a stored block advances nothing on the decoder's
    /// side. Leaving our table ahead of theirs would mistranslate every later
    /// repeat code — a divergence that produces wrong bytes, not an error.
    pub fn restore_repeats(&mut self, saved: [u32; REP_COUNT]) {
        self.repeats = saved;
    }

    /// Choose the `Offset_Value` for a sequence, and advance the repeat table.
    ///
    /// **The choice is made by asking the decoder.** §4 records the
    /// `literals_length == 0` shift as the most commonly botched part of the
    /// format, and it is not a rule worth re-deriving: each candidate is tried
    /// against [`resolve_offset`] on a copy of the table, and whichever
    /// reproduces the offset wins. An inverse written by hand could disagree in
    /// exactly the case §4 warns about, and the symptom would be a frame that
    /// decodes to plausible wrong bytes.
    fn choose_offset_value(&mut self, offset: u32, literal_length: u32) -> u32 {
        let (value, repeats) = offset_code_for(self.repeats, offset, literal_length == 0);
        self.repeats = repeats;
        value
    }

    /// Length of a match at one of the current repeat offsets, if it is usable.
    ///
    /// `floor` is the length the caller already has in hand. A repeat is only
    /// taken when it is **no shorter** than that, so a repeat that cannot reach
    /// `floor` need not be measured — and the byte at `floor - 1` settles it in
    /// one compare, where `common_prefix` would walk the whole agreement first.
    /// Three of these run for every position that found a match, and on
    /// structured text most of them lose.
    fn repeat_match(
        &self,
        input: &[u8],
        pos: usize,
        limit: usize,
        offset: u32,
        floor: usize,
    ) -> usize {
        let offset = offset as usize;
        // `reach`, not `pos`: a repeat that reached into the dictionary early
        // in the frame names nothing once the dictionary has dropped out.
        if offset == 0 || offset > self.reach(pos) {
            return 0;
        }
        let available = limit - pos;
        if available < MIN_MATCH || floor > available {
            return 0;
        }
        // In range: `floor <= available`, so `pos + floor - 1 < limit`, and
        // `offset <= pos` puts the source at or after the start of the input.
        if floor > 0 && input[pos - offset + floor - 1] != input[pos + floor - 1] {
            return 0;
        }
        B::common_prefix(&input[pos - offset..], &input[pos..pos + available])
    }

    /// Insert every position from the cursor up to, not including, `target`.
    ///
    /// The optimal parser's version of the loop invariant the lazy parse keeps
    /// by hand: whenever a position is searched, everything before it is in the
    /// chain and it is not. Positions with fewer than four bytes left have
    /// nothing to hash and are skipped, as [`Matcher::index_range`] skips them.
    fn insert_up_to(&mut self, input: &[u8], target: usize) {
        let hashable = input.len().saturating_sub(MIN_MATCH - 1);
        while self.indexed < target {
            let mut step = 1;
            if self.indexed < hashable {
                match self.finder {
                    // The fast finders never reach the optimal parser —
                    // `with_tuning` pairs them with greedy parsing only — and
                    // would index nothing here if they did.
                    Finder::Chain => self.insert(input, self.indexed),
                    Finder::Tree if self.deferred => self.add_unsorted(input, self.indexed),
                    Finder::Tree => step = self.tree_update(input, self.indexed, 0, 0, None),
                    Finder::Fast { .. } | Finder::DoubleFast { .. } | Finder::Row { .. } => {}
                }
            }
            // Never past `target`. The parser is about to search there, and
            // in a tree searching inserts: a skip that jumped over `target`
            // would leave it to be inserted *after* newer positions, as the
            // root of a tree that must be ordered by age. Clamped, a skip can
            // only ever leave positions out — which costs a match, never order.
            self.indexed += step.min(target - self.indexed);
        }
    }

    /// Search the binary tree at `pos` and insert `pos` into it, in one descent.
    ///
    /// libzstd's `ZSTD_insertBtAndGetAllMatches`, in shape. Each hash bucket
    /// is a binary search tree of earlier positions, ordered by the bytes that
    /// follow them, with the newest at the root. Inserting `pos` makes it the
    /// new root: the descent compares it against one node at a time, hangs each
    /// node below or above it accordingly, and steps into the side of that
    /// node's subtree where `pos` belongs. Every node visited is a candidate
    /// match, and because the tree is ordered, the nodes visited are the ones
    /// closest to `pos` in sort order — which is to say the longest matches.
    ///
    /// **The tree navigates; it is not trusted to measure.** A node's place in
    /// the tree implies it shares at least `min(common_below, common_above)`
    /// bytes with `pos`, and libzstd skips comparing those. So does this, to
    /// steer. But a match is *reported* only after a full comparison from its
    /// first byte. A tree made inconsistent by something unforeseen — a slot
    /// reused after the window wrapped, an ordering decided under a different
    /// limit — would otherwise report a length the bytes do not support, and
    /// the frame would decode wrong with nothing to say so. Reports happen only
    /// on an improvement, a handful per search, so the check costs little; an
    /// inconsistency costs a missed match, never a wrong one.
    ///
    /// With `out` absent this only inserts. With it, every match longer than
    /// `best` and no longer than `available` is appended, shortest first.
    ///
    /// Returns how many positions an insertion-only caller may advance — 1,
    /// or more after a long or self-overlapping match. See
    /// [`TREE_SKIP_AFTER`].
    fn tree_update(
        &mut self,
        input: &[u8],
        pos: usize,
        available: usize,
        best: usize,
        out: Option<&mut Vec<(u32, u32)>>,
    ) -> usize {
        let h = hash4(&input[pos..], self.hash_log);
        let node = self.head[h];
        self.head[h] = pos as u32;
        self.tree_descend(input, pos, node, pos, self.depth, available, best, out)
    }

    /// The descent of [`Matcher::tree_update`]: make `pos` the root of the
    /// tree whose old root is `node`, comparing at most `budget` nodes.
    ///
    /// Separate so the deferred tree can sort a position that is already in
    /// its bucket — see [`Matcher::find_tree`] — where the old root is the
    /// position's own link rather than the bucket's head, and the newest
    /// position written is `newest`, not `pos`.
    #[allow(clippy::too_many_arguments)]
    fn tree_descend(
        &mut self,
        input: &[u8],
        pos: usize,
        mut node: u32,
        newest: usize,
        budget: usize,
        available: usize,
        mut best: usize,
        mut out: Option<&mut Vec<(u32, u32)>>,
    ) -> usize {
        // Bytes compared for ordering. Never past the input, and never past
        // `TREE_COMPARE_MAX` — see there.
        let hi = input.len().min(pos + TREE_COMPARE_MAX);
        let floor = pos - self.reach(pos);
        // A node at or below this may share its slots with a newer position
        // once the tree has wrapped, so its children are not followed.
        let reused_below = newest.saturating_sub(self.tree_mask);

        // The two open edges: where the next node sorting below `pos` hangs,
        // and where the next one sorting above it does. They start as `pos`'s
        // own slots and move down the tree as the descent does. `None` once an
        // edge has run into a node whose children cannot be trusted.
        let own = 2 * (pos & self.tree_mask);
        let mut below = Some(own);
        let mut above = Some(own + 1);
        let mut common_below = 0usize;
        let mut common_above = 0usize;
        // The tree is a heap by age — every node is newer than its children —
        // so a real descent strictly decreases. Anything else is a stale slot.
        let mut previous = pos;
        // For the skip: the longest agreement seen, and the furthest any
        // matched source runs. The second is libzstd's test for a run whose
        // period is shorter than itself — a source ending past `pos` means the
        // positions after `pos` are copies of positions already in the tree.
        let mut longest = 0usize;
        let mut source_end = pos + 9;

        for _ in 0..budget {
            if node == EMPTY {
                break;
            }
            let m = node as usize;
            if m < floor || m >= previous {
                break;
            }
            previous = m;
            let slots = 2 * (m & self.tree_mask);

            let known = common_below.min(common_above);
            let mut length = known + B::common_prefix(&input[m + known..], &input[pos + known..hi]);
            longest = longest.max(length);
            source_end = source_end.max(m + length);

            if length > best && out.is_some() {
                // The verification described above: measured from byte zero.
                length = B::common_prefix(&input[m..], &input[pos..hi]);
                let reportable = length.min(available);
                if reportable > best
                    && reportable >= MIN_MATCH
                    && let Some(out) = out.as_mut()
                {
                    out.push((reportable as u32, (pos - m) as u32));
                    best = reportable;
                }
            }

            if pos + length >= hi {
                // Equal as far as the comparison goes, so there is no telling
                // which sorts first. Drop this node and its subtree from the
                // new tree rather than guess — libzstd's choice, and it costs a
                // little compression, never a wrong order.
                break;
            }

            if input[m + length] < input[pos + length] {
                // `m` sorts below `pos`: it hangs on the lower edge, and the
                // descent continues into the part of its subtree above it.
                if let Some(edge) = below {
                    self.tree[edge] = node;
                }
                common_below = length;
                if m <= reused_below {
                    below = None;
                    break;
                }
                below = Some(slots + 1);
                node = self.tree[slots + 1];
            } else {
                if let Some(edge) = above {
                    self.tree[edge] = node;
                }
                common_above = length;
                if m <= reused_below {
                    above = None;
                    break;
                }
                above = Some(slots);
                node = self.tree[slots];
            }
        }

        // Close both edges: whatever was not reached is no longer in the tree.
        if let Some(edge) = below {
            self.tree[edge] = EMPTY;
        }
        if let Some(edge) = above {
            self.tree[edge] = EMPTY;
        }

        let long = if longest > TREE_SKIP_AFTER {
            (longest - TREE_SKIP_AFTER).min(TREE_SKIP_MAX)
        } else {
            0
        };
        long.max(source_end - (pos + 8)).max(1)
    }

    /// Every match at `pos` worth pricing, shortest first, into `out`.
    ///
    /// Each entry is strictly longer than the one before it, so the list is the
    /// set of lengths reachable and, for each, the cheapest offset found that
    /// reaches it. The repeat offsets go first — they are the cheapest offsets
    /// to name — and a chain candidate is kept only if it is longer than
    /// anything already found.
    ///
    /// `repeats` and `literals_zero` are the decoder's state *along the path
    /// being priced*, which is why they are arguments: with no literals before
    /// the match, the three codable repeats shift by one (§4), and a parser
    /// that priced from the committed table instead would think the wrong
    /// offsets cheap.
    fn gather(
        &mut self,
        input: &[u8],
        pos: usize,
        limit: usize,
        repeats: [u32; REP_COUNT],
        literals_zero: bool,
        out: &mut Vec<(u32, u32)>,
    ) {
        out.clear();
        let available = limit.saturating_sub(pos);
        if available < MIN_MATCH {
            return;
        }
        // The length to beat. `best < available` holds throughout — the search
        // stops when a match reaches the limit — which is what keeps every
        // `[_ + best]` below in range.
        let mut best = MIN_MATCH - 1;

        let codable = if literals_zero {
            [repeats[1], repeats[2], repeats[0].wrapping_sub(1)]
        } else {
            repeats
        };
        for offset in codable {
            let offset = offset as usize;
            if offset == 0 || offset > self.reach(pos) {
                continue;
            }
            if input[pos - offset + best] != input[pos + best] {
                continue;
            }
            let length = B::common_prefix(&input[pos - offset..], &input[pos..pos + available]);
            if length > best {
                out.push((length as u32, offset as u32));
                best = length;
                if best == available || best >= self.target_length {
                    return;
                }
            }
        }

        if self.finder == Finder::Tree {
            // Searching the tree inserts `pos`, so the cursor moves past it
            // here — `insert_up_to` must not insert it a second time. Only on
            // this path: a return above, on a repeat that reached the target,
            // leaves `pos` for the cursor to insert later, as the chain does.
            debug_assert_eq!(self.indexed, pos, "searched out of order");
            self.tree_update(input, pos, available, best, Some(out));
            self.indexed = pos + 1;
            return;
        }

        let floor = pos - self.reach(pos);
        let mut candidate = self.head[hash4(&input[pos..], self.hash_log)];
        for _ in 0..self.depth {
            if candidate == EMPTY {
                break;
            }
            let c = candidate as usize;
            if c < floor || c >= pos {
                break;
            }
            let next = self.chain[c & self.chain_mask];
            if input[c + best] == input[pos + best] {
                let length = B::common_prefix(&input[c..], &input[pos..pos + available]);
                if length > best {
                    out.push((length as u32, (pos - c) as u32));
                    best = length;
                    if best == available || best >= self.target_length {
                        break;
                    }
                }
            }
            if next >= candidate {
                break;
            }
            candidate = next;
        }
    }

    /// Prices from the statistics as they stand.
    fn prices(&self) -> Prices {
        match &self.stats {
            Some(stats) => Prices::new(stats),
            None => Prices::new(&Stats::first_block(&[])),
        }
    }

    /// Get the statistics ready for the block `input[start..end]`.
    ///
    /// A two-pass level scouts the frame's first block: a separate matcher,
    /// identical but for its own empty tables, parses the block purely to learn
    /// what its literals and sequences look like, and this matcher starts from
    /// what the scout learned. libzstd's `btultra2` does the same, for the same
    /// reason — the first block's prices are otherwise a guess from raw byte
    /// counts, and every later block inherits them. The scout's sequences are
    /// discarded, and nothing it does touches this matcher's chain or repeat
    /// table, so it changes which path is preferred and never what is legal.
    fn prepare_statistics(&mut self, input: &[u8], start: usize, end: usize) {
        // The frame begins after the dictionary, not at 0.
        let first = self.dict_len;
        if let ParseMode::Optimal { passes: 2 } = self.mode
            && start == first
            && !self.scouted
        {
            let prefix = Prefix {
                len: self.dict_len,
                repeats: self.first_repeats,
            };
            let mut scout = Matcher::<B>::with_prefix(self.tuning, self.window_log, end, prefix);
            scout.prime(input);
            scout.scouted = true;
            let _ = scout.parse_optimal(input, start, end);
            self.stats = scout.stats;
            self.scouted = true;
            return;
        }
        let block = &input[start..end];
        match self.stats.as_mut() {
            None => self.stats = Some(Stats::first_block(block)),
            Some(stats) if start > first => stats.next_block(block),
            Some(_) => {}
        }
    }

    /// Commit one match: its literals, its sequence, and what it teaches the
    /// price model.
    #[allow(clippy::too_many_arguments)]
    fn emit_match(
        &mut self,
        input: &[u8],
        anchor: &mut usize,
        at: usize,
        length: usize,
        offset: usize,
        literals: &mut Vec<u8>,
        sequences: &mut Vec<Sequence>,
    ) {
        let literal_length = (at - *anchor) as u32;
        // Through the decoder's rule, whatever the parser assumed. The parser
        // tracks the same table per path and should agree; if it ever does
        // not, this is where the frame stays correct and only the price was
        // wrong.
        let offset_value = self.choose_offset_value(offset as u32, literal_length);
        let run = &input[*anchor..at];
        if let Some(stats) = self.stats.as_mut() {
            stats.record(run, length as u32, offset_value);
        }
        literals.extend_from_slice(run);
        sequences.push(Sequence {
            literal_length,
            match_length: length as u32,
            offset: offset as u32,
            offset_value,
        });
        *anchor = at + length;
    }

    /// Decompose `input[start..end]` by pricing paths rather than taking
    /// matches as they come.
    ///
    /// From each position that has a match, the parser walks forward filling a
    /// table: the cheapest known cost of reaching each later position, by a
    /// literal from the position before it or by any length of any match found
    /// at an earlier one. It stops when no path reaches further, when the table
    /// is [`OPT_NUM`] long, or when a match is long enough
    /// ([`Tuning::target_length`]) to take outright. Then it follows the
    /// cheapest path back and commits it.
    ///
    /// What the lazy parse cannot do and this can is give up a long match for a
    /// shorter one that leaves the input better placed — a repeat offset two
    /// bytes later, a literal now so that the next match is cheap. Every such
    /// trade is a price comparison, which is why the price model exists.
    fn parse_optimal(&mut self, input: &[u8], start: usize, end: usize) -> Parse {
        let span = end - start;
        let mut sequences = Vec::with_capacity(span / 16);
        let mut literals = Vec::with_capacity(span / 2);

        if span <= TAIL + MIN_MATCH {
            self.insert_up_to(input, end);
            literals.extend_from_slice(&input[start..end]);
            return Parse {
                sequences,
                literals,
            };
        }

        self.prepare_statistics(input, start, end);
        let mut prices = self.prices();

        let limit = end - TAIL;
        let target = self.target_length.min(OPT_NUM - 1);
        let mut opt = core::mem::take(&mut self.opt);
        let mut candidates = core::mem::take(&mut self.candidates);
        let mut path = core::mem::take(&mut self.path);

        let mut pos = start;
        let mut anchor = start;
        while pos + MIN_MATCH <= limit {
            self.insert_up_to(input, pos);
            self.gather(
                input,
                pos,
                limit,
                self.repeats,
                pos == anchor,
                &mut candidates,
            );
            let Some(&(longest, longest_offset)) = candidates.last() else {
                pos += 1;
                continue;
            };
            if longest as usize >= target {
                self.emit_match(
                    input,
                    &mut anchor,
                    pos,
                    longest as usize,
                    longest_offset as usize,
                    &mut literals,
                    &mut sequences,
                );
                pos = anchor;
                prices = self.prices();
                continue;
            }

            opt.clear();
            opt.push(Node {
                price: 0,
                match_length: 0,
                offset: 0,
                literals: (pos - anchor) as u32,
                repeats: self.repeats,
            });
            let mut last = 0usize;
            relax(&mut opt, 0, &candidates, &prices, &mut last);

            let mut cur = 1usize;
            while cur <= last {
                // Reach `cur` by one more literal. Charged the byte, plus the
                // change in the literals-length price the longer run implies.
                let prev = opt[cur - 1];
                let run = prev.literals;
                let price = prev.price
                    + i64::from(prices.literal(input[pos + cur - 1]))
                    + i64::from(prices.literal_length(run + 1))
                    - i64::from(prices.literal_length(run));
                if price < opt[cur].price {
                    opt[cur] = Node {
                        price,
                        match_length: 0,
                        offset: 0,
                        literals: run + 1,
                        repeats: prev.repeats,
                    };
                }
                if cur == last {
                    break;
                }
                if cur >= OPT_NUM {
                    last = cur;
                    break;
                }

                let here = pos + cur;
                if here + MIN_MATCH <= limit {
                    let node = opt[cur];
                    self.insert_up_to(input, here);
                    self.gather(
                        input,
                        here,
                        limit,
                        node.repeats,
                        node.literals == 0,
                        &mut candidates,
                    );
                    if let Some(&(length, offset)) = candidates.last() {
                        if length as usize >= target {
                            // Long enough to take without pricing the
                            // alternatives: end the stretch here, through it.
                            let reach = cur + length as usize;
                            ensure(&mut opt, reach);
                            let (_, repeats) =
                                offset_code_for(node.repeats, offset, node.literals == 0);
                            opt[reach] = Node {
                                price: node.price,
                                match_length: length,
                                offset,
                                literals: 0,
                                repeats,
                            };
                            last = reach;
                            break;
                        }
                        relax(&mut opt, cur, &candidates, &prices, &mut last);
                    }
                }
                cur += 1;
            }

            // Follow the cheapest path back from the furthest position, then
            // commit its matches in order. Literals along it are not emitted
            // here: they stay pending behind `anchor`, and a run that ends the
            // stretch carries into the next one.
            path.clear();
            let mut i = last;
            while i > 0 {
                let node = opt[i];
                if node.match_length > 0 {
                    let length = node.match_length as usize;
                    path.push((i - length, length, node.offset as usize));
                    i -= length;
                } else {
                    i -= 1;
                }
            }
            for &(at, length, offset) in path.iter().rev() {
                self.emit_match(
                    input,
                    &mut anchor,
                    pos + at,
                    length,
                    offset,
                    &mut literals,
                    &mut sequences,
                );
            }
            pos += last;
            prices = self.prices();
        }

        self.insert_up_to(input, end);
        literals.extend_from_slice(&input[anchor..end]);
        self.opt = opt;
        self.candidates = candidates;
        self.path = path;
        Parse {
            sequences,
            literals,
        }
    }

    /// Index every position in a range that has four bytes to hash.
    fn index_range(&mut self, input: &[u8], from: usize, to: usize) {
        if self.finder == Finder::Tree {
            // In order, from the cursor; `from` is already behind it.
            return self.insert_up_to(input, to);
        }
        let end = to.min(input.len().saturating_sub(MIN_MATCH - 1));
        // Every position, for the row finder too. libzstd's row finder indexes
        // only the first 96 and last 32 positions of a match over 384 bytes;
        // measured here (2026-10-09, `corpus/`) that rule cost 5–7% of size at
        // levels 5 and 12 — it lost the candidates repetitive input lives on.
        for pos in from..end {
            self.insert(input, pos);
        }
    }

    /// Decompose `input[start..end]` into sequences and literals.
    ///
    /// Earlier bytes of `input` are history: matches may reach into them, up to
    /// the window. `start` must be where the previous call ended, so the chain
    /// stays in step with the positions it names.
    pub fn parse(&mut self, input: &[u8], start: usize, end: usize) -> Parse {
        if let ParseMode::Optimal { .. } = self.mode {
            return self.parse_optimal(input, start, end);
        }
        match self.finder {
            Finder::Fast { min_match } => return self.parse_fast(input, start, end, min_match),
            Finder::DoubleFast { min_match, .. } => {
                return self.parse_double_fast(input, start, end, min_match);
            }
            // The row finder searches inside the shared greedy/lazy loop
            // below, through `find` and `insert`.
            Finder::Chain | Finder::Tree | Finder::Row { .. } => {}
        }
        // Both grow to some fraction of the block and both are rebuilt for
        // every block, so starting them at zero spends the block's first
        // kilobytes reallocating and copying. These are opening sizes, not
        // bounds — a block of pure literals still grows `literals` past the
        // guess, and a block that matches everything leaves it nearly empty.
        let span = end - start;
        let mut sequences = Vec::with_capacity(span / 16);
        let mut literals = Vec::with_capacity(span / 2);

        if end - start <= TAIL + MIN_MATCH {
            // Still index it, or the next block's matches cannot see it.
            self.index_range(input, start, end);
            literals.extend_from_slice(&input[start..end]);
            return Parse {
                sequences,
                literals,
            };
        }

        let limit = end - TAIL;
        let mut pos = start;
        let mut anchor = start;

        // Loop invariant: every position below `pos` has been inserted exactly
        // once, and none at or above it has. Both halves matter. Searching
        // before inserting is what keeps `pos` itself out of its own chain —
        // inserting first puts `pos` at the head, the walk stops on the very
        // first candidate because it is not *earlier*, and no match is ever
        // found. Inserting twice is worse: the second `insert` points a slot at
        // itself, and the chain from there is a self-loop.
        //
        // **Two rearrangements of this loop were measured and neither paid**;
        // see §10's M5 throughput notes. Walking the `pos` and `pos + 1`
        // searches in lockstep to overlap their cache misses, and carrying the
        // lookahead forward so a failed position does not search twice, both
        // came out inside run-to-run noise on a three-way alternating A/B. They
        // are recorded there rather than kept here, because each one costs this
        // invariant its simple form — the interleave in particular needs the
        // insert to lead the search, which is precisely the bug above.
        while pos + MIN_MATCH <= limit {
            let found = self.search(input, pos, limit);
            self.insert(input, pos);

            let Some((mut length, mut offset)) = found else {
                pos += 1;
                continue;
            };

            // Prefer a recent offset whenever it is no shorter. It costs a code
            // of 0 or 1 against up to seventeen bits for a literal offset, so an
            // equal-length match at a repeat is strictly the better sequence.
            // Only "no shorter" — trading match length for a cheaper offset is a
            // real trade, and making it well needs a cost model this encoder
            // does not have yet.
            for candidate in self.repeats {
                // `length.max(MIN_MATCH)` is exactly the bar the test below
                // sets, handed down so a repeat that cannot clear it is
                // dismissed on one byte rather than measured.
                let repeat_len =
                    self.repeat_match(input, pos, limit, candidate, length.max(MIN_MATCH));
                if repeat_len >= length && repeat_len >= MIN_MATCH {
                    length = repeat_len;
                    offset = candidate as usize;
                    break;
                }
            }

            // Lazy: a match a byte later may be long enough to be worth the
            // literal that taking it costs. Greedy levels take what they have;
            // lazy levels look one or two bytes ahead, moving forward each time
            // the next position does better. Each step that moves inserts the
            // position it leaves behind, which keeps the loop invariant — every
            // position below the one being searched is in, and it is not.
            let lookahead = match self.mode {
                ParseMode::Greedy => 0,
                ParseMode::Lazy(steps) => steps,
                ParseMode::Optimal { .. } => 0,
            };
            for _ in 0..lookahead {
                match self.search(input, pos + 1, limit) {
                    Some((next_len, next_offset)) if next_len >= length + LAZY_MARGIN => {
                        self.insert(input, pos + 1);
                        pos += 1;
                        length = next_len;
                        offset = next_offset;
                    }
                    _ => break,
                }
            }

            let literal_length = (pos - anchor) as u32;
            let offset_value = self.choose_offset_value(offset as u32, literal_length);
            literals.extend_from_slice(&input[anchor..pos]);
            sequences.push(Sequence {
                literal_length,
                match_length: length as u32,
                offset: offset as u32,
                offset_value,
            });

            // Index the interior of the match; `pos` is already in. Without
            // this, long matches leave holes and the ratio falls off sharply on
            // repetitive input.
            let match_end = pos + length;
            self.index_range(input, pos + 1, match_end);

            pos = match_end;
            anchor = match_end;
        }

        // Index whatever the loop did not reach, so the next block can see it.
        self.index_range(input, pos, end);

        literals.extend_from_slice(&input[anchor..end]);
        Parse {
            sequences,
            literals,
        }
    }

    /// Emit a sequence: the literals since `anchor`, then `length` bytes from
    /// `offset` back, its offset code chosen by asking the decoder.
    #[allow(clippy::too_many_arguments)]
    fn emit(
        &mut self,
        parse: &mut Parse,
        input: &[u8],
        anchor: usize,
        pos: usize,
        length: usize,
        offset: usize,
    ) {
        let literal_length = (pos - anchor) as u32;
        let offset_value = self.choose_offset_value(offset as u32, literal_length);
        parse.literals.extend_from_slice(&input[anchor..pos]);
        parse.sequences.push(Sequence {
            literal_length,
            match_length: length as u32,
            offset: offset as u32,
            offset_value,
        });
    }

    /// Whether `candidate` is a usable earlier position for a match at `pos`:
    /// present, before it, and inside the window.
    fn usable(&self, candidate: u32, pos: usize) -> bool {
        let c = candidate as usize;
        candidate != EMPTY && c < pos && pos - c <= self.reach(pos)
    }

    /// Length of a match at repeat `offset` from `pos`, if its first four bytes
    /// agree — libzstd's repcode check, which costs one compare when it fails.
    fn repeat_at(&self, input: &[u8], pos: usize, limit: usize, offset: u32) -> Option<usize> {
        let offset = offset as usize;
        if offset == 0 || offset > self.reach(pos) || pos + MIN_MATCH > limit {
            return None;
        }
        if four_at(input, pos - offset)? != four_at(input, pos)? {
            return None;
        }
        Some(B::common_prefix(&input[pos - offset..], &input[pos..limit]))
    }

    /// Extend a match backwards over literals it can absorb, as libzstd does:
    /// the position that found it is often a byte or two past where it begins.
    fn catch_up(
        input: &[u8],
        anchor: usize,
        mut pos: usize,
        mut length: usize,
        offset: usize,
    ) -> (usize, usize) {
        while pos > anchor && pos > offset && input[pos - 1] == input[pos - 1 - offset] {
            pos -= 1;
            length += 1;
        }
        (pos, length)
    }

    /// After a match ending at `pos`: while the second most recent offset
    /// matches again right here, take it with no literals — libzstd's
    /// "immediate repcode". Returns where the run of them ends.
    fn immediate_repeats(
        &mut self,
        parse: &mut Parse,
        input: &[u8],
        mut pos: usize,
        limit: usize,
        mut index: impl FnMut(&mut Self, &[u8], usize),
    ) -> usize {
        // After a sequence with literals the decoder's table is
        // [offset, older, oldest]; a sequence with none names `repeats[1]` by
        // code 1. `choose_offset_value` finds that code; this only proposes.
        while let Some(length) = self.repeat_at(input, pos, limit, self.repeats[1]) {
            let offset = self.repeats[1] as usize;
            index(self, input, pos);
            self.emit(parse, input, pos, pos, length, offset);
            pos += length;
        }
        pos
    }

    /// Level 1–2's parse: libzstd's `fast` (`zstd_fast.c`, 1.5.7).
    ///
    /// One table, one candidate. Each position searched is hashed and
    /// written; a miss steps on, one byte further for every 256 bytes since
    /// the last match; a match is extended backwards, taken, and leaves two
    /// positions indexed — two in, two from the end — before checking for an
    /// immediate repeat. Nothing else is inserted, which is most of the speed:
    /// the chain levels indexed every byte of every match.
    fn parse_fast(&mut self, input: &[u8], start: usize, end: usize, min_match: u8) -> Parse {
        let mut parse = Parse {
            sequences: Vec::with_capacity((end - start) / 32),
            literals: Vec::with_capacity((end - start) / 4),
        };
        let log = self.hash_log;
        let hash =
            |input: &[u8], pos: usize| word_at(input, pos).map(|w| hash_n(w, min_match, log));
        if end - start <= TAIL + MIN_MATCH {
            parse.literals.extend_from_slice(&input[start..end]);
            return parse;
        }
        let limit = end - TAIL;
        let mut pos = start;
        let mut anchor = start;
        while pos + MIN_MATCH <= limit {
            let Some(h) = hash(input, pos) else { break };
            let candidate = self.head[h];
            self.head[h] = pos as u32;
            let searched = pos;

            // The most recent offset, a byte on: the commonest match there is
            // in structured data, and the cheapest to code.
            let found = if let Some(length) = self.repeat_at(input, pos + 1, limit, self.repeats[0])
            {
                Some((pos + 1, length, self.repeats[0] as usize))
            } else if self.usable(candidate, pos)
                && four_at(input, candidate as usize) == four_at(input, pos)
            {
                let offset = pos - candidate as usize;
                let length = B::common_prefix(&input[candidate as usize..], &input[pos..limit]);
                let (at, length) = Self::catch_up(input, anchor, pos, length, offset);
                Some((at, length, offset))
            } else {
                None
            };
            let Some((at, length, offset)) = found else {
                pos += ((pos - anchor) >> SEARCH_STRENGTH) + 1;
                continue;
            };

            self.emit(&mut parse, input, anchor, at, length, offset);
            pos = at + length;
            anchor = pos;
            if pos <= limit {
                for p in [searched + 2, pos - 2] {
                    if let Some(h) = hash(input, p) {
                        self.head[h] = p as u32;
                    }
                }
                pos = self.immediate_repeats(&mut parse, input, pos, limit, |m, input, p| {
                    if let Some(h) = hash(input, p) {
                        m.head[h] = p as u32;
                    }
                });
                anchor = pos;
            }
        }
        parse.literals.extend_from_slice(&input[anchor..end]);
        parse
    }

    /// Level 3–4's parse: libzstd's `dfast` (`zstd_double_fast.c`, 1.5.7).
    ///
    /// Two tables, written at every position searched: a long one keyed on
    /// eight bytes, and a short one on `min_match`. A long hit is taken; a
    /// short one only after checking whether the long table has something
    /// longer a byte later. After a match, four positions are indexed and
    /// immediate repeats taken, as in [`Matcher::parse_fast`].
    fn parse_double_fast(
        &mut self,
        input: &[u8],
        start: usize,
        end: usize,
        min_match: u8,
    ) -> Parse {
        let mut parse = Parse {
            sequences: Vec::with_capacity((end - start) / 32),
            literals: Vec::with_capacity((end - start) / 4),
        };
        let (short_log, long_log) = (self.hash_log, self.long_log);
        let short = move |input: &[u8], pos: usize| {
            word_at(input, pos).map(|w| hash_n(w, min_match, short_log))
        };
        let long =
            move |input: &[u8], pos: usize| word_at(input, pos).map(|w| hash_n(w, 8, long_log));
        if end - start <= TAIL + MIN_MATCH {
            parse.literals.extend_from_slice(&input[start..end]);
            return parse;
        }
        let limit = end - TAIL;
        let mut pos = start;
        let mut anchor = start;
        while pos + MIN_MATCH <= limit {
            let (Some(hl), Some(hs)) = (long(input, pos), short(input, pos)) else {
                break;
            };
            let (cl, cs) = (self.long[hl], self.head[hs]);
            self.long[hl] = pos as u32;
            self.head[hs] = pos as u32;
            let searched = pos;

            let found = if let Some(length) = self.repeat_at(input, pos + 1, limit, self.repeats[0])
            {
                Some((pos + 1, length, self.repeats[0] as usize))
            } else if self.usable(cl, pos)
                && pos + 8 <= limit
                && word_at(input, cl as usize) == word_at(input, pos)
            {
                let offset = pos - cl as usize;
                let length = B::common_prefix(&input[cl as usize..], &input[pos..limit]);
                let (at, length) = Self::catch_up(input, anchor, pos, length, offset);
                Some((at, length, offset))
            } else if self.usable(cs, pos) && four_at(input, cs as usize) == four_at(input, pos) {
                let mut offset = pos - cs as usize;
                let mut at = pos;
                let mut length = B::common_prefix(&input[cs as usize..], &input[pos..limit]);
                // A long match a byte on beats the short one in hand.
                if let Some(h1) = long(input, pos + 1) {
                    let c1 = self.long[h1];
                    self.long[h1] = (pos + 1) as u32;
                    if self.usable(c1, pos + 1)
                        && pos + 1 + 8 <= limit
                        && word_at(input, c1 as usize) == word_at(input, pos + 1)
                    {
                        let l1 = B::common_prefix(&input[c1 as usize..], &input[pos + 1..limit]);
                        if l1 > length {
                            (at, length, offset) = (pos + 1, l1, pos + 1 - c1 as usize);
                        }
                    }
                }
                let (at, length) = Self::catch_up(input, anchor, at, length, offset);
                Some((at, length, offset))
            } else {
                None
            };
            let Some((at, length, offset)) = found else {
                pos += ((pos - anchor) >> SEARCH_STRENGTH) + 1;
                continue;
            };

            self.emit(&mut parse, input, anchor, at, length, offset);
            pos = at + length;
            anchor = pos;
            if pos <= limit {
                let p = searched + 2;
                if let (Some(hl), Some(hs)) = (long(input, p), short(input, p)) {
                    self.long[hl] = p as u32;
                    self.head[hs] = p as u32;
                }
                if let Some(h) = long(input, pos - 2) {
                    self.long[h] = (pos - 2) as u32;
                }
                if let Some(h) = short(input, pos - 1) {
                    self.head[h] = (pos - 1) as u32;
                }
                pos = self.immediate_repeats(&mut parse, input, pos, limit, |m, input, p| {
                    if let (Some(hl), Some(hs)) = (long(input, p), short(input, p)) {
                        m.long[hl] = p as u32;
                        m.head[hs] = p as u32;
                    }
                });
                anchor = pos;
            }
        }
        parse.literals.extend_from_slice(&input[anchor..end]);
        parse
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    // The tests measure with the reference backend; the equivalence tests in
    // `zalt-neon` hold every other backend to the same frames.
    type Matcher = super::Matcher<crate::Scalar>;

    fn parse_one(block: &[u8]) -> Parse {
        Matcher::new(17, block.len()).parse(block, 0, block.len())
    }

    /// A parse must account for every byte of the block, exactly once.
    ///
    /// The invariant the whole sequence section rests on: the decoder rebuilds
    /// a block as literals and matches interleaved, so if the lengths do not
    /// sum to the input, the output is the wrong length and every byte after
    /// the first discrepancy is wrong too.
    fn assert_accounts_for(block: &[u8], parse: &Parse) {
        let matched: u64 = parse
            .sequences
            .iter()
            .map(|s| u64::from(s.match_length))
            .sum();
        let claimed: u64 = parse
            .sequences
            .iter()
            .map(|s| u64::from(s.literal_length))
            .sum();
        assert!(
            claimed <= parse.literals.len() as u64,
            "sequences claim more literals than exist"
        );
        assert_eq!(
            matched + parse.literals.len() as u64,
            block.len() as u64,
            "matches plus literals must be the block"
        );
    }

    /// Replay a parse the way the decoder would, over given history.
    fn replay(history: &[u8], parse: &Parse) -> Vec<u8> {
        let mut out = history.to_vec();
        let base = out.len();
        let mut lit = 0usize;
        for s in &parse.sequences {
            let n = s.literal_length as usize;
            out.extend_from_slice(&parse.literals[lit..lit + n]);
            lit += n;
            let start = out.len() - s.offset as usize;
            // Byte at a time: a match may overlap its own output, which is the
            // whole point of a small offset.
            for i in 0..s.match_length as usize {
                let b = out[start + i];
                out.push(b);
            }
        }
        out.extend_from_slice(&parse.literals[lit..]);
        out.split_off(base)
    }

    /// The literal-offset shortcut in `offset_code_for` gives exactly what
    /// asking the decoder for every code gives, over a grid of repeat tables,
    /// offsets and both literal-length cases.
    #[test]
    fn offset_codes_take_the_shortcut_only_where_it_agrees_with_the_decoder() {
        let slow = |repeats: [u32; REP_COUNT], offset: u32, zero: bool| {
            for candidate in 1..=REP_COUNT as u32 {
                let mut trial = repeats;
                if resolve_offset(candidate, zero, &mut trial) == Ok(offset) {
                    return (candidate, trial);
                }
            }
            let mut trial = repeats;
            resolve_offset(offset + 3, zero, &mut trial).unwrap();
            (offset + 3, trial)
        };
        for a in 1..7 {
            for b in 1..7 {
                for c in 1..7 {
                    for offset in 1..10 {
                        for zero in [false, true] {
                            let r = [a, b, c];
                            assert_eq!(
                                offset_code_for(r, offset, zero),
                                slow(r, offset, zero),
                                "{r:?} {offset} {zero}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// The SWAR tag match agrees with comparing byte by byte, at every row
    /// width, for tags whose bit patterns stress the trick: 0x00 and 0xFF, and
    /// 0x80 and 0x7F, where a borrow or carry between bytes would show.
    #[test]
    fn tag_matching_agrees_with_comparing_each_byte() {
        let mut s = 0x9E37_79B9u32;
        let mut next = || {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s >> 24) as u8
        };
        for entries in [16usize, 32, 64] {
            for round in 0..2000 {
                let tag = match round % 5 {
                    0 => 0x00,
                    1 => 0xFF,
                    2 => 0x80,
                    3 => 0x7F,
                    _ => next(),
                };
                // Rows dense with the tag and its neighbours, and random ones.
                let row: Vec<u8> = (0..entries)
                    .map(|_| match next() % 4 {
                        0 => tag,
                        1 => tag.wrapping_add(1),
                        2 => tag ^ 0x80,
                        _ => next(),
                    })
                    .collect();
                let expected = row
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| **b == tag)
                    .fold(0u64, |m, (i, _)| m | 1 << i);
                assert_eq!(tag_matches(&row, tag), expected, "{row:02x?} tag {tag:02x}");
            }
        }
    }

    /// Levels 5–15 (the lazy parse over rows, then over a tree) account for every
    /// byte and replay to the input across blocks. For the tree this is also
    /// the in-order check: a position searched by the lookahead and then
    /// indexed again would corrupt the tree, and with the tree's own verified
    /// reporting that shows up here as matches missing, not wrong bytes.
    #[test]
    fn the_lazy_finders_parse_and_replay_across_blocks() {
        let mut input = Vec::new();
        for i in 0..400u32 {
            input.extend_from_slice(format!("line {} of the log: status ok\n", i % 37).as_bytes());
        }
        input.extend(core::iter::repeat_n(0x41u8, 3000));
        let mut s = 0x1234_5678u32;
        input.extend((0..5000).map(|_| {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s >> 24) as u8
        }));
        let copy = input[100..4000].to_vec();
        input.extend_from_slice(&copy);
        // 13–15 as a tree, though the table keeps chains there: the lazy
        // loop's tree path must hold up whether or not a level uses it.
        for level in 5..=15 {
            let mut tuning = super::super::level::tuning(level);
            if level >= 13 {
                tuning.finder = Finder::Tree;
                tuning.search_depth = 64;
            }
            assert!(
                matches!(tuning.finder, Finder::Row { .. } | Finder::Tree),
                "level {level}"
            );
            let mut m = Matcher::with_tuning(tuning, 22, input.len());
            let mut start = 0;
            let mut matched = 0;
            while start < input.len() {
                let end = (start + 3001).min(input.len());
                let parse = m.parse(&input, start, end);
                assert_accounts_for(&input[start..end], &parse);
                assert_eq!(
                    replay(&input[..start], &parse),
                    &input[start..end],
                    "level {level} at {start}"
                );
                matched += parse
                    .sequences
                    .iter()
                    .map(|q| q.match_length as usize)
                    .sum::<usize>();
                start = end;
            }
            // About 80% of the input is compressible — the log lines, the run
            // and the copy; only the noise is not. A finder that silently
            // found nothing would still replay, so hold it to finding most.
            assert!(
                matched * 10 >= input.len() * 6,
                "level {level} matched only {matched} of {} bytes",
                input.len()
            );
        }
    }

    /// The deferred tree with a window far smaller than the input, so its
    /// slots are reused over and over: positions in a bucket's unsorted list
    /// and in its tree go stale under it. That may cost matches; it must never
    /// cost bytes, an offset past the window, or a panic.
    #[test]
    fn the_deferred_tree_survives_a_wrapping_window() {
        let mut input = Vec::new();
        for i in 0..800u32 {
            input.extend_from_slice(format!("line {} of the log: status ok\n", i % 37).as_bytes());
        }
        input.extend(core::iter::repeat_n(0x41u8, 3000));
        let copy = input[100..9000].to_vec();
        input.extend_from_slice(&copy);
        for depth in [2, 16, 64] {
            let tuning = Tuning {
                finder: Finder::Tree,
                search_depth: depth,
                ..super::super::level::tuning(13)
            };
            let mut m = Matcher::with_tuning(tuning, 10, input.len());
            assert!(m.deferred);
            let mut start = 0;
            let mut matched = 0;
            while start < input.len() {
                let end = (start + 3001).min(input.len());
                let parse = m.parse(&input, start, end);
                assert_accounts_for(&input[start..end], &parse);
                assert_eq!(
                    replay(&input[..start], &parse),
                    &input[start..end],
                    "depth {depth} at {start}"
                );
                for q in &parse.sequences {
                    assert!(q.offset <= 1 << 10, "depth {depth}: offset {}", q.offset);
                    matched += q.match_length as usize;
                }
                start = end;
            }
            // Consecutive log lines share most of their bytes, well inside
            // the window, so even a tree losing positions finds most of it.
            assert!(
                matched * 10 >= input.len() * 6,
                "depth {depth} matched only {matched} of {}",
                input.len()
            );
        }
    }

    /// Levels 1–4 (the fast finders) account for every byte and replay to the
    /// input, block after block over shared history — text, runs, noise and
    /// near-repeats, so every branch of both loops is taken.
    #[test]
    fn the_fast_finders_parse_and_replay_across_blocks() {
        let mut input = Vec::new();
        for i in 0..400u32 {
            input.extend_from_slice(format!("line {} of the log: status ok\n", i % 37).as_bytes());
        }
        input.extend(core::iter::repeat_n(0x41u8, 3000));
        let mut s = 0x1234_5678u32;
        input.extend((0..5000).map(|_| {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s >> 24) as u8
        }));
        let copy = input[100..4000].to_vec();
        input.extend_from_slice(&copy);
        for level in 1..=4 {
            let tuning = super::super::level::tuning(level);
            assert!(matches!(
                tuning.finder,
                Finder::Fast { .. } | Finder::DoubleFast { .. }
            ));
            let mut m = Matcher::with_tuning(tuning, 22, input.len());
            let mut start = 0;
            while start < input.len() {
                let end = (start + 3001).min(input.len());
                let parse = m.parse(&input, start, end);
                assert_accounts_for(&input[start..end], &parse);
                assert_eq!(
                    replay(&input[..start], &parse),
                    &input[start..end],
                    "level {level} at {start}"
                );
                start = end;
            }
        }
    }

    #[test]
    fn a_repetitive_block_parses_and_replays() {
        let block = b"abcdefgh".repeat(200);
        let parse = parse_one(&block);
        assert_accounts_for(&block, &parse);
        assert!(!parse.sequences.is_empty());
        assert_eq!(replay(&[], &parse), block);
    }

    #[test]
    fn overlapping_matches_replay_correctly() {
        let block = vec![0x41u8; 1000];
        let parse = parse_one(&block);
        assert_accounts_for(&block, &parse);
        assert_eq!(replay(&[], &parse), block);
        assert!(parse.sequences.iter().any(|s| s.offset == 1));
    }

    #[test]
    fn incompressible_input_produces_no_bogus_matches() {
        let block: Vec<u8> = (0..4000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        let parse = parse_one(&block);
        assert_accounts_for(&block, &parse);
        assert_eq!(replay(&[], &parse), block);
    }

    #[test]
    fn short_blocks_are_all_literals() {
        for n in 0..=(TAIL + MIN_MATCH) {
            let block = vec![7u8; n];
            let parse = parse_one(&block);
            assert!(parse.sequences.is_empty(), "{n} bytes");
            assert_eq!(parse.literals, block, "{n} bytes");
        }
    }

    #[test]
    fn a_second_block_matches_into_the_first() {
        // The point of carrying history: the same bytes again should cost
        // almost nothing, and cannot unless offsets cross the block boundary.
        //
        // The content has no *internal* repetition, deliberately. Repeated
        // prose would let the second block match 44 bytes back instead of
        // reaching into the first, and the test would pass while proving
        // nothing about history.
        let half: Vec<u8> = (0..4000u32)
            .scan(12345u32, |st, _| {
                *st = st.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                Some((*st >> 24) as u8)
            })
            .collect();
        let mut input = half.clone();
        input.extend_from_slice(&half);

        let mut matcher = Matcher::new(17, 1 << 17);
        let first = matcher.parse(&input, 0, half.len());
        let second = matcher.parse(&input, half.len(), input.len());

        assert_accounts_for(&half, &first);
        assert_accounts_for(&half, &second);
        assert_eq!(replay(&[], &first), half);
        assert_eq!(replay(&input[..half.len()], &second), half);

        // Every match in the second block must reach back roughly a block, as
        // there is nowhere nearer for one to come from.
        assert!(
            second
                .sequences
                .iter()
                .any(|s| s.offset as usize >= half.len() / 2),
            "the second block should reach back into the first"
        );
        // The first block is incompressible; the second is a verbatim copy of
        // it, so almost all of it should be matched.
        assert!(
            second.literals.len() * 8 < first.literals.len(),
            "a verbatim repeat should need far fewer literals: {} then {}",
            first.literals.len(),
            second.literals.len()
        );
    }

    #[test]
    fn no_offset_reaches_before_the_start_of_the_input() {
        let block = b"the quick brown fox the quick brown fox the quick".repeat(30);
        let parse = parse_one(&block);
        let mut produced = 0u64;
        for s in &parse.sequences {
            produced += u64::from(s.literal_length);
            assert!(s.offset >= 1, "offset must be positive");
            assert!(
                u64::from(s.offset) <= produced,
                "offset {} reaches before the start at {produced}",
                s.offset
            );
            produced += u64::from(s.match_length);
        }
    }

    #[test]
    fn a_declared_window_bounds_every_offset() {
        // A small window must be honoured, or the frame names history the
        // decoder has already dropped.
        let block = b"abcdefghijklmnop".repeat(500);
        let window_log = 10u8;
        let parse = Matcher::new(window_log, block.len()).parse(&block, 0, block.len());
        assert_accounts_for(&block, &parse);
        assert_eq!(replay(&[], &parse), block);
        for s in &parse.sequences {
            assert!(
                (s.offset as usize) <= (1usize << window_log),
                "offset {} exceeds the declared window",
                s.offset
            );
        }
    }

    #[test]
    fn a_block_always_ends_with_literals() {
        let block = b"abcabcabc".repeat(500);
        let parse = parse_one(&block);
        let claimed: usize = parse
            .sequences
            .iter()
            .map(|s| s.literal_length as usize)
            .sum();
        assert!(
            parse.literals.len() > claimed,
            "the trailing literal run must not be empty"
        );
    }

    /// Whatever offset value is chosen must resolve back to the offset asked
    /// for, and leave the matcher's repeat table where the decoder's will be.
    ///
    /// The property the whole repeat mechanism rests on, and both halves
    /// matter: a value that resolves correctly but advances the table
    /// differently is right for one sequence and wrong for every sequence after
    /// it. That is the shape of failure §4 warns about — plausible output, no
    /// error, and the divergence invisible until something compares bytes.
    #[test]
    fn every_chosen_offset_value_round_trips_with_its_table() {
        // Runs of one offset, alternating offsets, and zero-literal sequences,
        // where the meaning of values 1-3 shifts by one.
        let plan: &[(u32, u32)] = &[
            (17, 5),
            (17, 0),
            (17, 3),
            (4, 0),
            (17, 0),
            (4, 2),
            (4, 0),
            (99, 1),
            (17, 0),
            (1, 0),
            (1, 4),
            (2, 0),
            (99, 0),
        ];

        let mut matcher = Matcher::new(17, 1 << 17);
        let mut theirs = REP_INITIAL;
        let mut repeats_used = 0usize;

        for &(offset, literal_length) in plan {
            let value = matcher.choose_offset_value(offset, literal_length);
            if value <= REP_COUNT as u32 {
                repeats_used += 1;
            }
            let resolved = resolve_offset(value, literal_length == 0, &mut theirs);
            assert_eq!(
                resolved,
                Ok(offset),
                "value {value} did not resolve to offset {offset} (ll {literal_length})"
            );
            assert_eq!(
                matcher.repeats(),
                theirs,
                "the tables diverged after offset {offset}"
            );
        }

        assert!(
            repeats_used >= 4,
            "a plan full of repeated offsets should use repeat codes, used {repeats_used}"
        );
    }

    #[test]
    fn a_repeated_offset_costs_almost_nothing() {
        // The point of the mechanism: the second use of an offset names it
        // rather than spelling out the seventeen bits a 128 KiB offset needs.
        let mut matcher = Matcher::new(17, 1 << 17);
        assert_eq!(
            matcher.choose_offset_value(100_000, 5),
            100_003,
            "the first use must be a literal offset"
        );
        assert_eq!(
            matcher.choose_offset_value(100_000, 5),
            1,
            "the second use must name the most recent offset"
        );
    }

    #[test]
    fn a_rolled_back_table_is_exactly_the_one_saved() {
        // What a block falling back to stored depends on.
        let mut matcher = Matcher::new(17, 1 << 17);
        matcher.choose_offset_value(40, 2);
        let saved = matcher.repeats();
        matcher.choose_offset_value(999, 0);
        matcher.choose_offset_value(12, 7);
        assert_ne!(matcher.repeats(), saved, "the table should have moved");
        matcher.restore_repeats(saved);
        assert_eq!(matcher.repeats(), saved);
    }

    #[test]
    fn the_search_prefers_a_recent_offset_when_it_is_no_shorter() {
        // Two sources give an equally long match; the one already in the repeat
        // table should win, because naming it costs a code of 0 or 1.
        let mut block = Vec::new();
        block.extend_from_slice(b"PATTERNPATTERNPATTERN");
        block.extend_from_slice(&[b'.'; 40]);
        block.extend_from_slice(b"PATTERNPATTERNPATTERN");
        block.extend_from_slice(&[b'.'; 40]);
        block.extend_from_slice(b"PATTERNPATTERNPATTERN");
        block.extend_from_slice(&[b';'; 32]);

        let parse = parse_one(&block);
        assert_accounts_for(&block, &parse);
        assert_eq!(replay(&[], &parse), block);
        assert!(
            parse.sequences.iter().any(|s| s.offset_value <= 3),
            "a repeating pattern should reach for a repeat code: {:?}",
            parse.sequences
        );
    }

    #[test]
    fn chains_find_a_longer_match_than_the_most_recent_candidate() {
        // A short recent occurrence and a long older one share a prefix. One
        // hash slot takes the recent, shorter match; a chain finds both and
        // keeps the longer.
        let mut block = Vec::new();
        block.extend_from_slice(b"LONGMATCHTARGETXXXXXXXXXXXXXXXXXX");
        block.extend_from_slice(&[b'.'; 26]);
        block.extend_from_slice(b"LONGMATCHTARGETyy");
        block.extend_from_slice(&[b','; 26]);
        block.extend_from_slice(b"LONGMATCHTARGETXXXXXXXXXXXXXXXXXX");
        block.extend_from_slice(&[b';'; 32]);

        let parse = parse_one(&block);
        assert_accounts_for(&block, &parse);
        assert_eq!(replay(&[], &parse), block);
        assert!(
            parse.sequences.iter().any(|s| s.match_length > 20),
            "the older, longer match should have been found: {:?}",
            parse.sequences
        );
    }

    // ------------------------------------------------- the optimal parser

    /// A matcher at the top level, which parses optimally with a scout pass,
    /// finding candidates the level's way — a binary tree.
    fn optimal(window_log: u8, len: usize) -> Matcher {
        Matcher::with_tuning(crate::encode::level::tuning(19), window_log, len)
    }

    /// The same, but with the hash chain. No level uses this pairing today; it
    /// is kept working, and tested, because it is the simpler of the two and
    /// the one a tree bug would be bisected against.
    fn optimal_chain(window_log: u8, len: usize) -> Matcher {
        let tuning = Tuning {
            finder: Finder::Chain,
            ..crate::encode::level::tuning(19)
        };
        Matcher::with_tuning(tuning, window_log, len)
    }

    /// Every shape the lazy parse is tested on, through the optimal parser.
    ///
    /// It shares the lazy parse's invariants and none of its code: it inserts
    /// positions through its own cursor, searches out of the lazy loop's order,
    /// and commits a path it found by walking backwards. So each of these is a
    /// separate claim about it, not a repeat of one already made.
    #[test]
    fn the_optimal_parse_accounts_for_and_replays_every_shape() {
        let incompressible: Vec<u8> = (0..4000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        let mut patterned = Vec::new();
        for _ in 0..3 {
            patterned.extend_from_slice(b"PATTERNPATTERNPATTERN");
            patterned.extend_from_slice(&[b'.'; 40]);
        }
        patterned.extend_from_slice(&[b';'; 32]);
        let shapes: [(&str, Vec<u8>); 6] = [
            ("repetitive", b"abcdefgh".repeat(200)),
            ("one byte", alloc::vec![0x41u8; 1000]),
            ("incompressible", incompressible),
            (
                "prose",
                b"the quick brown fox the quick brown fox the quick".repeat(30),
            ),
            ("patterned", patterned),
            ("short", b"tiny".to_vec()),
        ];
        for ((name, block), finder) in shapes
            .iter()
            .flat_map(|shape| [(shape, "tree"), (shape, "chain")])
        {
            let mut matcher = if finder == "tree" {
                optimal(17, block.len())
            } else {
                optimal_chain(17, block.len())
            };
            let parse = matcher.parse(block, 0, block.len());
            let name = &format!("{name} ({finder})");
            assert_accounts_for(block, &parse);
            assert_eq!(&replay(&[], &parse), block, "{name}");
            let mut produced = 0u64;
            for s in &parse.sequences {
                produced += u64::from(s.literal_length);
                assert!(s.match_length >= 3, "{name}: match below the format floor");
                assert!(s.offset >= 1, "{name}: zero offset");
                assert!(
                    u64::from(s.offset) <= produced,
                    "{name}: reaches before the start"
                );
                produced += u64::from(s.match_length);
            }
        }
    }

    /// The table's repeat state per path must agree with the decoder's at every
    /// committed sequence — checked by replaying the offset *values*, not the
    /// offsets, through the decoder's own rule.
    #[test]
    fn the_optimal_parse_writes_offset_values_the_decoder_resolves_correctly() {
        let mut block = Vec::new();
        for i in 0..400u32 {
            block.extend_from_slice(format!("<row n=\"{}\">{}</row>\n", i % 17, i % 5).as_bytes());
        }
        let parse = optimal(17, block.len()).parse(&block, 0, block.len());
        let mut theirs = REP_INITIAL;
        let mut repeats_used = 0;
        for s in &parse.sequences {
            let resolved = resolve_offset(s.offset_value, s.literal_length == 0, &mut theirs);
            assert_eq!(
                resolved,
                Ok(s.offset),
                "value {} for offset {}",
                s.offset_value,
                s.offset
            );
            if s.offset_value <= REP_COUNT as u32 {
                repeats_used += 1;
            }
        }
        assert!(
            repeats_used > 0,
            "structured rows should reach for repeat codes"
        );
        assert_eq!(replay(&[], &parse), block);
    }

    /// **The tree navigates; it is never trusted to measure.**
    ///
    /// The claim [`Matcher::tree_update`] rests its safety on, tested the only
    /// way it can be: by breaking the tree. Every child link is overwritten
    /// with an arbitrary earlier position — an ordering the tree's invariants
    /// forbid and libzstd would trust — and every match reported afterwards
    /// must still be real, byte for byte. A version that reported the length
    /// the tree *implied* would fail here on its first skipped prefix.
    #[test]
    fn a_corrupted_tree_still_reports_only_real_matches() {
        let mut block = Vec::new();
        for i in 0..600u32 {
            block.extend_from_slice(format!("<k{}>{}</k{}>", i % 9, i % 31, i % 9).as_bytes());
        }
        let mut matcher = optimal(17, block.len());
        let half = block.len() / 2;
        matcher.insert_up_to(&block, half);

        let mut s = 0x2545_F491u32;
        for slot in 0..matcher.tree.len() {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let owner = slot / 2;
            if owner > 0 && owner < half {
                // Strictly older than the slot's owner, so the age guard does
                // not catch it, and arbitrary otherwise.
                matcher.tree[slot] = (s as usize % owner) as u32;
            }
        }

        let limit = block.len() - TAIL;
        let mut out = Vec::new();
        let mut reported = 0usize;
        for pos in half..limit - MIN_MATCH {
            matcher.insert_up_to(&block, pos);
            matcher.gather(&block, pos, limit, REP_INITIAL, false, &mut out);
            for &(length, offset) in &out {
                let (length, offset) = (length as usize, offset as usize);
                assert!(offset >= 1 && offset <= pos, "offset {offset} at {pos}");
                assert!(pos + length <= limit, "past the limit at {pos}");
                assert_eq!(
                    block[pos - offset..pos - offset + length],
                    block[pos..pos + length],
                    "a {length}-byte match at offset {offset} from {pos} is not real"
                );
                reported += 1;
            }
        }
        assert!(
            reported > 100,
            "the corrupted tree should still find matches: {reported}"
        );
    }

    /// Every position goes into the tree once, in order, unless a skip left it
    /// out — and a skip never jumps past where the parser will search next.
    #[test]
    fn tree_insertion_never_passes_the_search_position() {
        let block = b"abababababababababababababababab-the-run-ends-here-".repeat(80);
        let mut matcher = optimal(17, block.len());
        let mut out = Vec::new();
        let limit = block.len() - TAIL;
        let mut pos = 0;
        while pos + MIN_MATCH <= limit {
            matcher.insert_up_to(&block, pos);
            assert_eq!(matcher.indexed, pos, "the cursor overshot {pos}");
            matcher.gather(&block, pos, limit, REP_INITIAL, false, &mut out);
            pos += 7;
        }
    }

    /// History across blocks, and a window that refuses some of it.
    #[test]
    fn the_optimal_parse_reaches_across_blocks_and_honours_the_window() {
        let half: Vec<u8> = (0..4000u32)
            .scan(12345u32, |st, _| {
                *st = st.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                Some((*st >> 24) as u8)
            })
            .collect();
        let mut input = half.clone();
        input.extend_from_slice(&half);

        let mut matcher = optimal(17, input.len());
        let first = matcher.parse(&input, 0, half.len());
        let second = matcher.parse(&input, half.len(), input.len());
        assert_eq!(replay(&[], &first), half);
        assert_eq!(replay(&input[..half.len()], &second), half);
        assert!(
            second.literals.len() * 8 < first.literals.len(),
            "a verbatim repeat a block back should cost few literals: {} then {}",
            first.literals.len(),
            second.literals.len()
        );

        // A 1 KiB window cannot see 4000 bytes back, so the copy must not be
        // found — and nothing may reach past the window to find it anyway.
        let mut small = optimal(10, input.len());
        let _ = small.parse(&input, 0, half.len());
        let second = small.parse(&input, half.len(), input.len());
        assert_eq!(replay(&input[..half.len()], &second), half);
        for s in &second.sequences {
            assert!(
                s.offset <= 1 << 10,
                "offset {} past a 1 KiB window",
                s.offset
            );
        }
    }

    /// The lazy parse over a tree (libzstd's `btlazy2`) finds nearly what an
    /// exhaustive chain does, and deferring the sort costs next to nothing.
    ///
    /// The tree must see each position exactly once and in order, and the
    /// lazy loop searches some positions and indexes others; the deferred
    /// tree must also sort what a search walks, oldest first. A slip in any of
    /// it corrupts the tree, which reports only verified matches — so it shows
    /// as cost, never as wrong bytes. Measured on this input: the tree sorted
    /// on insert 1.3% over a chain searched 4096 deep, and 30% over with each
    /// lazily-searched position inserted twice; deferred, 0.08% from sorted
    /// on insert, against 1.2% with the walk sorted newest first and 3.7%
    /// with it not sorted at all.
    #[test]
    fn the_tree_finds_what_a_deep_chain_finds() {
        let mut input = Vec::new();
        for i in 0..6000u32 {
            input.extend_from_slice(
                format!(
                    "  <entry id=\"{i}\" kind=\"{}\">value {}</entry>\n",
                    i % 7,
                    (i * 13) % 101
                )
                .as_bytes(),
            );
        }
        let cost = |tuning: Tuning, deferred: bool| {
            let mut m = Matcher::with_tuning(tuning, 22, input.len());
            assert_eq!(m.deferred, tuning.finder == Finder::Tree);
            m.deferred = deferred;
            let (mut literals, mut sequences) = (0, 0);
            let mut start = 0;
            while start < input.len() {
                let end = (start + (32 << 10)).min(input.len());
                let parse = m.parse(&input, start, end);
                assert_accounts_for(&input[start..end], &parse);
                assert_eq!(replay(&input[..start], &parse), &input[start..end]);
                literals += parse.literals.len();
                sequences += parse.sequences.len();
                start = end;
            }
            literals * 8 + sequences * 24
        };
        let tree = Tuning {
            finder: Finder::Tree,
            search_depth: 16,
            ..super::super::level::tuning(13)
        };
        let chain = Tuning {
            finder: Finder::Chain,
            search_depth: 4096,
            ..tree
        };
        let (sorted, deferred, chain) = (cost(tree, false), cost(tree, true), cost(chain, false));
        assert!(
            sorted * 100 <= chain * 105,
            "sorted on insert {sorted} against chain {chain}"
        );
        assert!(
            deferred.abs_diff(sorted) * 1000 <= sorted * 5,
            "deferred {deferred} against sorted on insert {sorted}"
        );
    }

    /// The optimal parser must not lose to the lazy one on ordinary structured
    /// text.
    ///
    /// Not a theorem — its prices are estimates, and an estimate can steer it
    /// wrong — but on content like this a loss means the price model is broken,
    /// and a broken price model is invisible to every correctness test here:
    /// the frames still decode. M5 recorded two bugs of exactly that kind, each
    /// found only because something asserted a size.
    #[test]
    fn the_optimal_parse_is_no_larger_than_the_lazy_parse_on_structured_text() {
        let mut block = Vec::new();
        for i in 0..2000u32 {
            block.extend_from_slice(
                format!(
                    "  <entry id=\"{i}\" kind=\"{}\">value {}</entry>\n",
                    i % 7,
                    (i * 13) % 101
                )
                .as_bytes(),
            );
        }
        let size = |level| {
            let params = crate::encode::EncodeParams {
                strategy: crate::encode::Strategy::Compress,
                level,
                ..Default::default()
            };
            crate::encode::compress(&block, &params)
                .expect("encodable")
                .len()
        };
        let (lazy, optimal) = (size(15), size(19));
        assert!(
            optimal <= lazy,
            "level 19 {optimal} bytes against level 15 {lazy}"
        );
    }
}
