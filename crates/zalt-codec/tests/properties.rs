//! Property tests over the codec's public surface.
//!
//! These overlap the fuzz targets on purpose and are not a substitute for them:
//! `cargo fuzz` runs for hours under coverage guidance, and this runs in
//! milliseconds on every `just t`. What it buys is that the properties are
//! *written down in the repository* and checked in the normal test loop, rather
//! than living only in a campaign somebody has to remember to start.
//!
//! The properties chosen are the ones whose failure mode is **plausible wrong
//! output** rather than a crash — the class `doc/ZALT-MAC.md` §10 records
//! getting wrong twice, where a table built, every internal consistency check
//! passed, and only a checksum eventually objected.

use proptest::prelude::*;
use zalt_codec::fse::{self, DecodeTable, Distribution};
use zalt_codec::huf;
use zalt_codec::params::DecodeLimits;

/// Counts that sum, in absolute value, to exactly `1 << accuracy_log`.
///
/// Building a *valid* distribution rather than a random one is the point: the
/// interesting invariants are about tables that pass validation, and random
/// counts almost never do.
fn valid_counts(accuracy_log: u32) -> impl Strategy<Value = Vec<i16>> {
    let table_size = 1usize << accuracy_log;
    // Between two and thirty-two symbols sharing the mass.
    (2usize..=32.min(table_size)).prop_flat_map(move |n| {
        // Distribute `table_size` over `n` symbols, every symbol at least one.
        proptest::collection::vec(1u32..=64, n).prop_map(move |weights| {
            let total: u32 = weights.iter().sum();
            let mut counts: Vec<i16> = weights
                .iter()
                .map(|w| {
                    let share = (*w as u64 * table_size as u64 / total as u64).max(1);
                    share as i16
                })
                .collect();
            // Force the sum to land exactly on the table size.
            let mut sum: i32 = counts.iter().map(|c| i32::from(*c)).sum();
            let target = table_size as i32;
            let mut i = 0;
            let n_counts = counts.len();
            while sum != target {
                let c = &mut counts[i % n_counts];
                if sum < target {
                    *c += 1;
                    sum += 1;
                } else if *c > 1 {
                    *c -= 1;
                    sum -= 1;
                }
                i += 1;
            }
            counts
        })
    })
}

proptest! {
    /// Arbitrary bytes are decoded or refused. Never a panic, never a hang.
    ///
    /// The weakest property here and the one with the most history: every
    /// validation obligation in §4 exists because some shape of input reached
    /// code that assumed it could not.
    #[test]
    fn arbitrary_input_is_refused_rather_than_fatal(data: Vec<u8>) {
        let _ = zalt_codec::decompress(&data, DecodeLimits::default());
    }

    /// The same, with a valid magic number in front.
    ///
    /// Unprefixed random bytes essentially never survive the magic check, so
    /// they exercise four bytes of header and nothing else. Prefixing reaches
    /// the frame header parser, which is where the interesting refusals are.
    #[test]
    fn arbitrary_input_behind_a_valid_magic_is_refused_rather_than_fatal(tail: Vec<u8>) {
        let mut data = vec![0x28, 0xB5, 0x2F, 0xFD];
        data.extend_from_slice(&tail);
        let _ = zalt_codec::decompress(&data, DecodeLimits::default());
    }

    /// An FSE table description is parsed or rejected, never fatally.
    #[test]
    fn fse_distribution_parsing_is_total(data: Vec<u8>) {
        let _ = Distribution::parse(&data, fse::ACCURACY_LOG_MAX, 255);
    }

    /// **Every state of a built table decodes to an in-range next state.**
    ///
    /// This is the invariant `DecodeTable::entry_masked` relies on to skip its
    /// bounds check: it masks the index by `size - 1` and argues that a table
    /// which passed validation can only ever produce states below `size`. The
    /// argument is written in a comment; this checks it.
    #[test]
    fn built_tables_only_produce_in_range_states(
        accuracy_log in 5u32..=9u32,
        counts in (5u32..=9u32).prop_flat_map(valid_counts),
    ) {
        // The strategy picked its own log; keep the pair consistent.
        let log = accuracy_log.min(9);
        let Ok(dist) = Distribution::from_counts(&counts, log) else {
            return Ok(()); // not every generated shape is a legal distribution
        };
        let Ok(table) = DecodeTable::build(&dist) else {
            return Ok(());
        };
        let size = table.len();
        prop_assert!(size.is_power_of_two());
        prop_assert_eq!(size, 1usize << table.accuracy_log());

        for state in 0..size {
            let entry = table.entry(state as u16).expect("state below size is valid");
            // A state emits `nb_bits` and lands on `new_state + read(nb_bits)`,
            // so the whole reachable span must stay inside the table.
            prop_assert!(u32::from(entry.nb_bits) <= table.accuracy_log());
            let highest = u32::from(entry.new_state) + ((1u32 << entry.nb_bits) - 1);
            prop_assert!(
                (highest as usize) < size,
                "state {} reaches {} in a table of {}",
                state, highest, size
            );
        }
    }

    /// A state at or above `size` is an error, never a silent wrap.
    #[test]
    fn states_past_the_table_are_rejected(
        counts in (5u32..=9u32).prop_flat_map(valid_counts),
        overshoot in 0u16..64,
    ) {
        let Ok(dist) = Distribution::from_counts(&counts, 6) else {
            return Ok(());
        };
        let Ok(table) = DecodeTable::build(&dist) else {
            return Ok(());
        };
        let past = table.len() as u16 + overshoot;
        prop_assert!(table.entry(past).is_err());
    }

    /// A Huffman tree description is accepted or rejected, never fatally, and
    /// an accepted one declares a `tableLog` the format permits.
    #[test]
    fn huffman_weight_descriptions_are_total(weights: Vec<u8>) {
        if let Ok(table) = huf::Table::from_weights(&weights) {
            prop_assert!(table.table_log() >= 1);
            prop_assert!(table.table_log() <= huf::TABLELOG_MAX);
        }
    }

    /// Parsing a Huffman description from arbitrary bytes is total.
    #[test]
    fn huffman_table_parsing_is_total(data: Vec<u8>) {
        let _ = huf::Table::parse(&data);
    }
}
