//! Compression levels: what each one asks the match finder to do.
//!
//! ## Shape borrowed from libzstd, numbers measured here
//!
//! A level is a bundle of match-finder knobs — how far back to look, how many
//! candidates to try, and how to choose among them. The *progression* follows
//! libzstd's `clevels.h` (1.5.7, read from the vendored source, not recalled):
//! wider windows and deeper searches as the level rises, greedy parsing at the
//! bottom, lazy evaluation in the middle, and optimal parsing at the top. The
//! *values* do not transfer, because the strategies behind them differ — zalt
//! has one hash-chain finder where libzstd has six — so they were set by
//! measuring this encoder against libzstd at the same level (`zalt-bench
//! encode-levels`), not copied.
//!
//! ## Levels 1 to 19
//!
//! libzstd goes to 22. Levels 20–22 differ from 19 chiefly by declaring 32, 64
//! and 128 MiB windows, which is past the 8 MiB ceiling §6 sets for untrusted
//! decoding and which every browser shares — a frame a browser must refuse. They
//! are deliberately not distinguished yet: a level above 19 is treated as 19,
//! which is what libzstd itself does with a level above its own maximum.
//!
//! Level 0 means the default, as it does in libzstd. Negative levels — libzstd's
//! "fast" range — are treated as level 1.
//!
//! ## What a level cannot do
//!
//! Nothing here can change whether a frame is *correct*. A level picks which
//! matches are looked for and which are taken; the offset code written for each
//! is still chosen by asking the decoder's own `resolve_offset`, and the
//! sequence encoding below the parse is shared by every level. A bad table entry
//! costs ratio or time, never bytes.

/// Lowest level with distinct behaviour.
pub const LEVEL_MIN: i32 = 1;

/// Highest level with distinct behaviour. See the module comment for 20–22.
pub const LEVEL_MAX: i32 = 19;

/// The level `0` stands for, and the library default — libzstd's, too.
pub const LEVEL_DEFAULT: i32 = 3;

/// How a level chooses among the matches it finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parse {
    /// Take the first match found at each position.
    Greedy,
    /// Before taking a match, check whether one starting a byte later is
    /// better, up to this many bytes ahead (1 or 2).
    Lazy(u8),
    /// Price every way of covering a stretch of input — literals, each match
    /// length, each offset — and keep the cheapest path. `passes` of 2 parses
    /// the first block once purely to learn its statistics.
    Optimal {
        /// 1, or 2 to seed the price model from a discarded first parse.
        passes: u8,
    },
}

/// How candidate matches are found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finder {
    /// A hash chain: every earlier position with the same four-byte hash, most
    /// recent first. Cheap to maintain, and each step is one candidate.
    Chain,
    /// A binary tree per hash bucket, ordered by the bytes following each
    /// position — libzstd's `bt` family. Each step halves what is left to
    /// search, so it reaches the longest matches in far fewer steps; it costs a
    /// tree update on every position. Optimal parsing only.
    Tree,
}

/// The knobs one level sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tuning {
    /// Window to search, as a log2. The frame may declare less: never more than
    /// the caller's ceiling ([`super::EncodeParams::window_log`]), and never
    /// more than the content needs.
    pub window_log: u8,
    /// Largest hash table, as a log2. Also capped by the input's size.
    pub hash_log: u32,
    /// Chain candidates examined per position.
    pub search_depth: usize,
    /// How candidates are chosen among.
    pub parse: Parse,
    /// How candidates are found. [`Finder::Tree`] requires
    /// [`Parse::Optimal`]; the lazy parse searches positions it then may not
    /// insert, which a tree — where searching *is* inserting — cannot do.
    pub finder: Finder,
    /// A match at least this long is taken without further search. Bounds the
    /// optimal parser's cost on long matches, where pricing every length gains
    /// nothing; `usize::MAX` for "never stop early".
    pub target_length: usize,
}

const fn t(
    window_log: u8,
    hash_log: u32,
    search_depth: usize,
    parse: Parse,
    target: usize,
) -> Tuning {
    Tuning {
        window_log,
        hash_log,
        search_depth,
        parse,
        finder: Finder::Chain,
        target_length: target,
    }
}

/// The same knobs, searched with a binary tree.
const fn tree(tuning: Tuning) -> Tuning {
    Tuning {
        finder: Finder::Tree,
        ..tuning
    }
}

const NO_TARGET: usize = usize::MAX;

/// Levels 1 to 19, in order.
///
/// Windows top out at 23 (8 MiB), libzstd's value for level 19 and §6's
/// untrusted-decode ceiling, so no level emits a frame a browser must refuse.
///
/// The top of the table was set by ablation (sizes against the same libzstd
/// level, time likewise; `doc/ZALT-MAC.md` §10 has the full tables):
///
/// - **16 to 19 search with a binary tree.** With hash chains, depth was the
///   only lever on large input and an expensive one — level 19 at 256
///   candidates reached 1.012x `zstd -19`'s size at 3.9x its time. The tree
///   reaches 1.003x at 1.1x the time, and moves levels 16 and 17 *below*
///   libzstd's size (0.990x, 0.997x). Its depth hardly matters (32 to 256
///   measured within 0.05%), because a tree descent ends at a leaf long before
///   the budget; what mattered was how far it compares before giving up on
///   ordering two positions — see `TREE_COMPARE_MAX` in the match finder.
/// - **The scout pass buys little** — 0.15% on small documents, for roughly
///   double the time there. Kept at 19 only, the level that promises maximum
///   effort.
/// - **Small documents barely care**: every variant tried came within about
///   0.3% of the others on the ZTML corpus.
const TABLE: [Tuning; 19] = [
    t(19, 14, 2, Parse::Greedy, NO_TARGET),                  //  1
    t(20, 16, 4, Parse::Greedy, NO_TARGET),                  //  2
    t(21, 17, 6, Parse::Greedy, NO_TARGET),                  //  3
    t(21, 18, 8, Parse::Lazy(1), NO_TARGET),                 //  4
    t(21, 18, 12, Parse::Lazy(1), NO_TARGET),                //  5
    t(21, 19, 16, Parse::Lazy(1), NO_TARGET),                //  6
    t(21, 19, 24, Parse::Lazy(2), NO_TARGET),                //  7
    t(21, 20, 32, Parse::Lazy(2), NO_TARGET),                //  8
    t(22, 20, 48, Parse::Lazy(2), NO_TARGET),                //  9
    t(22, 21, 64, Parse::Lazy(2), NO_TARGET),                // 10
    t(22, 21, 96, Parse::Lazy(2), NO_TARGET),                // 11
    t(22, 22, 128, Parse::Lazy(2), NO_TARGET),               // 12
    t(22, 22, 192, Parse::Lazy(2), NO_TARGET),               // 13
    t(22, 22, 256, Parse::Lazy(2), NO_TARGET),               // 14
    t(22, 22, 384, Parse::Lazy(2), NO_TARGET),               // 15
    tree(t(22, 22, 32, Parse::Optimal { passes: 1 }, 48)),   // 16
    tree(t(23, 22, 48, Parse::Optimal { passes: 1 }, 64)),   // 17
    tree(t(23, 22, 128, Parse::Optimal { passes: 1 }, 128)), // 18
    tree(t(23, 22, 128, Parse::Optimal { passes: 2 }, 256)), // 19
];

/// The level a request resolves to: 0 is the default, and the rest clamp to
/// [`LEVEL_MIN`]`..=`[`LEVEL_MAX`].
pub const fn effective(level: i32) -> i32 {
    if level == 0 {
        LEVEL_DEFAULT
    } else if level < LEVEL_MIN {
        LEVEL_MIN
    } else if level > LEVEL_MAX {
        LEVEL_MAX
    } else {
        level
    }
}

/// The knobs for a level.
pub const fn tuning(level: i32) -> Tuning {
    TABLE[(effective(level) - LEVEL_MIN) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_level_resolves_and_the_edges_clamp() {
        assert_eq!(effective(0), LEVEL_DEFAULT);
        assert_eq!(effective(-5), LEVEL_MIN);
        assert_eq!(effective(22), LEVEL_MAX);
        assert_eq!(effective(i32::MAX), LEVEL_MAX);
        assert_eq!(effective(i32::MIN), LEVEL_MIN);
        for level in LEVEL_MIN..=LEVEL_MAX {
            assert_eq!(effective(level), level);
            let _ = tuning(level);
        }
    }

    /// No level may declare a window a browser must refuse.
    #[test]
    fn no_level_exceeds_the_untrusted_window_ceiling() {
        for level in LEVEL_MIN..=LEVEL_MAX {
            assert!(tuning(level).window_log <= 23, "level {level}");
        }
    }

    /// Effort must not fall as the level rises. A table that searched less at
    /// level 12 than at 11 would be a typo, and one that is invisible: every
    /// frame would still decode.
    #[test]
    fn effort_is_monotone_within_each_parse_family() {
        for pair in TABLE.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            assert!(b.window_log >= a.window_log, "{a:?} -> {b:?}");
            if core::mem::discriminant(&a.parse) == core::mem::discriminant(&b.parse) {
                assert!(b.search_depth >= a.search_depth, "{a:?} -> {b:?}");
                assert!(b.target_length >= a.target_length, "{a:?} -> {b:?}");
            }
        }
    }
}
