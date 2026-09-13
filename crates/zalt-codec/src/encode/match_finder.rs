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
//!   first [`SEARCH_DEPTH`]. One slot means the *newest* candidate wins, which
//!   is frequently not the longest.
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

use crate::sequences::{REP_COUNT, REP_INITIAL, resolve_offset};

/// Shortest match worth emitting.
///
/// The format's floor is 3, from match-length code 0. Four is used because the
/// hash reads four bytes anyway, and a three-byte match rarely pays for the
/// sequence describing it.
const MIN_MATCH: usize = 4;

/// Bytes at the end of a block left as literals rather than matched into.
///
/// The hash reads four bytes at a time, so the last few positions have nothing
/// to hash. It also guarantees a block ends with a literal run, which keeps the
/// sequence section's trailing literals non-degenerate.
const TAIL: usize = 8;

/// Candidates examined per position before settling.
///
/// Cost is linear in this and the benefit falls off quickly.
const SEARCH_DEPTH: usize = 32;

/// A later match must beat the one in hand by this much to be worth the literal
/// that taking it costs.
const LAZY_MARGIN: usize = 1;

/// Largest hash table the matcher will build, as a log2.
///
/// Also the *only* size it used to build. A 2^17 table of `u32` is 512 KiB, and
/// with a chain of the same order that is around a megabyte zeroed per frame —
/// which §8's workload pays once per resource, on resources of a few kilobytes.
/// The M5 profile found `memset` at 7.6% of a 7.7 KB encode, all of it clearing
/// table space for positions that do not exist.
const HASH_LOG_MAX: u32 = 17;

/// Smallest hash table worth building.
///
/// Below a few thousand slots the table stops separating anything and the chain
/// walk does all the work, spending its [`SEARCH_DEPTH`] budget on collisions
/// instead of candidates.
const HASH_LOG_MIN: u32 = 12;

/// Hash bits for an input of this size.
///
/// One slot per input position, clamped. A 4 KB resource cannot fill more than
/// 4096 slots however large the table is, so sizing to the content is not a
/// tuning compromise on small inputs — it is declining to clear memory that
/// provably stays empty. Inputs at or above 128 KB get the full table and are
/// unaffected.
fn hash_log_for(len: usize) -> u32 {
    let needed = usize::BITS - len.max(1).leading_zeros();
    needed.clamp(HASH_LOG_MIN, HASH_LOG_MAX)
}

/// Marks an empty slot. Unreachable as a position: a frame this encoder emits
/// is bounded well below 4 GiB.
const EMPTY: u32 = u32::MAX;

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

/// How far two slices agree, from their starts.
///
/// **A word at a time**, which is where the encoder's time was going. This is
/// the innermost loop of the whole encoder — [`Matcher::find`] runs it up to
/// [`SEARCH_DEPTH`] times per position, twice over for the lazy step — and a
/// byte-at-a-time `take_while` walks it with a compare and an unpredictable
/// branch per byte. Comparing eight bytes at once turns that into an XOR and a
/// `trailing_zeros`, which on Firestorm is `eor`/`rbit`/`clz`: three
/// instructions for what was eight iterations.
///
/// `trailing_zeros` finds the lowest *bit* that differs, and because the load is
/// little-endian the lowest differing bit lies in the lowest differing byte —
/// so `>> 3` is the byte index, with no endianness correction. A big-endian host
/// would need `leading_zeros` instead, which is why the load is explicitly
/// `from_le_bytes` rather than native.
///
/// No `unsafe`, and no bounds check in the loop either: `first_chunk` returns
/// `None` at the end of the shorter slice, which is also the loop's exit
/// condition. `matched` only advances after *both* chunks are produced, so the
/// slicing below it can never be out of range.
fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    const WORD: usize = 8;

    let mut matched = 0usize;
    while let (Some(x), Some(y)) = (
        a[matched..].first_chunk::<WORD>(),
        b[matched..].first_chunk::<WORD>(),
    ) {
        let diff = u64::from_le_bytes(*x) ^ u64::from_le_bytes(*y);
        if diff != 0 {
            return matched + (diff.trailing_zeros() >> 3) as usize;
        }
        matched += WORD;
    }

    // Fewer than eight bytes left on one side. At most seven iterations.
    matched
        + a[matched..]
            .iter()
            .zip(&b[matched..])
            .take_while(|(x, y)| x == y)
            .count()
}

/// A match finder whose history persists across the blocks of one frame.
pub struct Matcher {
    /// Most recent position for each hash.
    head: Vec<u32>,
    /// Bits of hash `head` is indexed by. Sized to the input — see
    /// [`hash_log_for`].
    hash_log: u32,
    /// Previous position sharing a hash, indexed by position modulo the window.
    chain: Vec<u32>,
    chain_mask: usize,
    /// Largest offset the frame's declared window admits.
    max_offset: usize,
    /// The three recent offsets, as the decoder will hold them.
    ///
    /// Advanced through [`crate::sequences::resolve_offset`] so it cannot drift
    /// from the decoder's copy. Reset per *frame*, which is why it lives with
    /// the matcher rather than being rebuilt per block.
    repeats: [u32; REP_COUNT],
}

impl Matcher {
    /// A matcher for a frame with this `windowLog`, encoding `input_len` bytes.
    ///
    /// **`input_len` sizes the tables and nothing else.** It is not a limit —
    /// every offset is still bounded by the window, and a caller that passes a
    /// larger figure gets a larger table and the same output.
    pub fn new(window_log: u8, input_len: usize) -> Self {
        let window = 1usize << window_log.min(31);
        // The chain is indexed modulo its own size, so entries older than the
        // window are overwritten and drop out — which is exactly the set of
        // positions an offset could no longer reach anyway.
        //
        // Capping it at the input length as well is free of consequence rather
        // than a trade: a chain at least as long as the input never has two
        // live positions sharing a slot, so nothing that could have been found
        // in the larger table is missing from this one. It only declines to
        // clear slots no position can reach.
        let chain_size = window.next_power_of_two().min(
            input_len
                .max(1)
                .checked_next_power_of_two()
                .unwrap_or(window),
        );
        let hash_log = hash_log_for(input_len);
        Self {
            head: vec![EMPTY; 1 << hash_log],
            hash_log,
            chain: vec![EMPTY; chain_size],
            chain_mask: chain_size - 1,
            max_offset: window,
            repeats: REP_INITIAL,
        }
    }

    fn insert(&mut self, input: &[u8], pos: usize) {
        let h = hash4(&input[pos..], self.hash_log);
        self.chain[pos & self.chain_mask] = self.head[h];
        self.head[h] = pos as u32;
    }

    /// Longest match at `pos`, searching back through the chain.
    ///
    /// Returns `(length, offset)`. `limit` is the position matching may not run
    /// past — the block's tail boundary.
    fn find(&self, input: &[u8], pos: usize, limit: usize) -> Option<(usize, usize)> {
        let available = limit.saturating_sub(pos);
        if available < MIN_MATCH {
            return None;
        }
        let floor = pos.saturating_sub(self.max_offset);

        let mut best_len = 0usize;
        let mut best_offset = 0usize;
        // The byte one past the end of the best match — what a candidate has to
        // agree with to be worth comparing. Held rather than re-loaded from
        // `input` for every candidate. Never read while `best_len` is zero.
        let mut best_tail = 0u8;
        let mut candidate = self.head[hash4(&input[pos..], self.hash_log)];

        for _ in 0..SEARCH_DEPTH {
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
                let length = common_prefix(&input[c..], &input[pos..pos + available]);
                if length > best_len {
                    best_len = length;
                    best_offset = pos - c;
                    if best_len == available {
                        // Nothing can beat a match that runs to the limit.
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
        for candidate in 1..=REP_COUNT as u32 {
            let mut trial = self.repeats;
            if resolve_offset(candidate, literal_length == 0, &mut trial) == Ok(offset) {
                self.repeats = trial;
                return candidate;
            }
        }
        let value = offset + REP_COUNT as u32;
        let mut trial = self.repeats;
        let resolved = resolve_offset(value, literal_length == 0, &mut trial);
        debug_assert_eq!(resolved, Ok(offset));
        self.repeats = trial;
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
        if offset == 0 || offset > pos {
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
        common_prefix(&input[pos - offset..], &input[pos..pos + available])
    }

    /// Index every position in a range that has four bytes to hash.
    fn index_range(&mut self, input: &[u8], from: usize, to: usize) {
        let end = to.min(input.len().saturating_sub(MIN_MATCH - 1));
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
            let found = self.find(input, pos, limit);
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

            // Lazy: a match one byte later may be long enough to be worth the
            // literal that taking it costs. One step only — a second lookahead
            // doubles the search for very little.
            if let Some((next_len, next_offset)) = self.find(input, pos + 1, limit)
                && next_len >= length + LAZY_MARGIN
            {
                self.insert(input, pos + 1);
                pos += 1;
                length = next_len;
                offset = next_offset;
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
