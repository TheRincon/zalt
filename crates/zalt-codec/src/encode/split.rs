//! Choosing where blocks end (§7's block splitting).
//!
//! ## What the measurements said before any of this was written
//!
//! §7 proposes splitting at statistical transitions so each block gets a
//! Huffman table fitted to one kind of content. The M4 caution asked for the
//! premise to be checked first, and checking it changed the design:
//!
//! - **Splitting at a fixed stride is a loss on every real file.** Compressed
//!   size falls monotonically as blocks grow — 4 KiB blocks cost 3% over
//!   128 KiB ones. Each block pays for a Huffman weight description and three
//!   FSE table descriptions, and smaller blocks also give those tables less
//!   data to fit against.
//! - **The mechanism does work, on content that has the structure.** 64 KiB of
//!   text followed by 64 KiB of random bytes compresses 1.6% smaller when split
//!   at exactly that boundary.
//! - **Real content does not have the structure.** The same bytes interleaved
//!   at 64-byte granularity — which is what a document carrying a hash per
//!   subresource looks like — gain 0.1%, which is noise.
//!
//! So this does not split; it *detects*. A boundary is proposed only where the
//! byte distribution actually changes, and the caller encodes both ways and
//! keeps the smaller. On content with no transition the detector returns
//! nothing and nothing is encoded twice, which is what keeps the common case
//! from paying for a feature it cannot use.
//!
//! ## Why the statistic is a histogram distance and not entropy
//!
//! Entropy is the natural measure and needs a logarithm, which `core` does not
//! have — this crate is `no_std` and deliberately float-free. The L1 distance
//! between two byte histograms needs only integer arithmetic, and for "did the
//! kind of data change" it discriminates just as well: two windows of English
//! text agree closely, and text against random bytes does not.

use alloc::vec::Vec;

/// Window over which one histogram is taken.
///
/// Small enough to place a boundary usefully, large enough that the histogram
/// is not mostly sampling noise: 256 bins over 4 KiB averages sixteen hits a
/// bin.
const WINDOW: usize = 4096;

/// Shortest block the detector will propose.
///
/// A block shorter than this cannot repay its own table descriptions, which the
/// stride sweep measured at roughly 150 to 300 bytes.
const MIN_BLOCK: usize = 16 * 1024;

/// How different two adjacent windows must be, in L1 distance over their
/// histograms, as a fraction of the maximum possible distance in 256ths.
///
/// The maximum is `2 * WINDOW` — two distributions sharing no byte value at
/// all. Tuned so that text against random bytes fires and two windows of the
/// same prose do not.
const THRESHOLD_256THS: u64 = 154; // 0.60

fn histogram(bytes: &[u8]) -> [u32; 256] {
    let mut counts = [0u32; 256];
    for &b in bytes {
        counts[usize::from(b)] += 1;
    }
    counts
}

/// L1 distance between two histograms taken over equally sized windows.
fn distance(a: &[u32; 256], b: &[u32; 256]) -> u64 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| u64::from(x.abs_diff(*y)))
        .sum()
}

/// Boundaries at which `input` changes character, in ascending order.
///
/// Excludes 0 and `input.len()`; every returned value is a position at which a
/// block should end. Empty when nothing in the input justifies a split, which
/// is the answer for every file in this repository's corpus.
///
/// `max_block` bounds the blocks the caller will emit, so a boundary is also
/// proposed wherever one would otherwise be exceeded — the caller needs a
/// complete set of boundaries, not only the interesting ones.
pub fn transitions(input: &[u8], max_block: usize) -> Vec<usize> {
    let mut points = Vec::new();
    if input.len() < 2 * MIN_BLOCK {
        return points;
    }

    let mut previous: Option<[u32; 256]> = None;
    let mut last_boundary = 0usize;
    let mut found_any = false;

    let mut start = 0usize;
    while start + WINDOW <= input.len() {
        let current = histogram(&input[start..start + WINDOW]);
        if let Some(prev) = previous {
            let d = distance(&prev, &current);
            let limit = (2 * WINDOW as u64) * THRESHOLD_256THS / 256;
            // A boundary is only useful where both sides can carry their own
            // tables, and only where the caller has not already been forced to
            // break the block.
            if d > limit && start - last_boundary >= MIN_BLOCK && input.len() - start >= MIN_BLOCK {
                points.push(start);
                last_boundary = start;
                found_any = true;
            }
        }
        previous = Some(current);
        start += WINDOW;
    }

    if !found_any {
        // No transition, so no reason for the caller to encode anything twice.
        return Vec::new();
    }

    // Fill in the boundaries the block size forces, so the returned list is a
    // complete description of where blocks end.
    let mut complete = Vec::with_capacity(points.len() + input.len() / max_block + 1);
    let mut cursor = 0usize;
    for point in points {
        while point - cursor > max_block {
            cursor += max_block;
            complete.push(cursor);
        }
        complete.push(point);
        cursor = point;
    }
    while input.len() - cursor > max_block {
        cursor += max_block;
        complete.push(cursor);
    }
    complete
}

/// Boundaries for plain fixed-size blocks.
pub fn fixed(len: usize, block_size: usize) -> Vec<usize> {
    let mut points = Vec::new();
    let mut cursor = block_size;
    while cursor < len {
        points.push(cursor);
        cursor += block_size;
    }
    points
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn text() -> Vec<u8> {
        b"the quick brown fox jumps over the lazy dog. ".repeat(2000)
    }

    fn noise(seed: u32, len: usize) -> Vec<u8> {
        let mut s = seed;
        (0..len)
            .map(|_| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (s >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn uniform_content_proposes_nothing() {
        // The answer for every file in the corpus, and the reason the common
        // case never pays for this.
        assert!(transitions(&text()[..80 * 1024], 128 * 1024).is_empty());
        assert!(transitions(&noise(1, 80 * 1024), 128 * 1024).is_empty());
    }

    #[test]
    fn a_real_transition_is_found_near_where_it_happens() {
        let mut input = text()[..64 * 1024].to_vec();
        input.extend_from_slice(&noise(9, 64 * 1024));
        let points = transitions(&input, 128 * 1024);

        assert!(!points.is_empty(), "the change of content should be seen");
        let nearest = points
            .iter()
            .min_by_key(|p| p.abs_diff(64 * 1024))
            .expect("non-empty");
        assert!(
            nearest.abs_diff(64 * 1024) <= WINDOW,
            "boundary at {nearest} is not near the transition at {}",
            64 * 1024
        );
    }

    #[test]
    fn boundaries_are_ascending_and_inside_the_input() {
        let mut input = text()[..64 * 1024].to_vec();
        input.extend_from_slice(&noise(3, 64 * 1024));
        input.extend_from_slice(&text()[..64 * 1024]);
        let points = transitions(&input, 32 * 1024);

        let mut previous = 0usize;
        for &p in &points {
            assert!(p > previous, "boundaries must ascend: {points:?}");
            assert!(p < input.len(), "boundary past the end: {p}");
            previous = p;
        }
    }

    #[test]
    fn no_proposed_block_exceeds_the_maximum() {
        let mut input = text()[..64 * 1024].to_vec();
        input.extend_from_slice(&noise(5, 200 * 1024));
        let max = 32 * 1024;
        let points = transitions(&input, max);
        if points.is_empty() {
            return;
        }
        let mut cursor = 0usize;
        for &p in &points {
            assert!(p - cursor <= max, "block of {} exceeds {max}", p - cursor);
            cursor = p;
        }
        assert!(input.len() - cursor <= max, "final block exceeds {max}");
    }

    #[test]
    fn fixed_boundaries_tile_the_input() {
        assert_eq!(fixed(10, 4), vec![4, 8]);
        assert_eq!(fixed(8, 4), vec![4]);
        assert_eq!(fixed(3, 4), vec![]);
        assert_eq!(fixed(0, 4), vec![]);
    }

    #[test]
    fn a_tiny_input_is_never_split() {
        // Below twice the minimum block there is nowhere a split could pay.
        assert!(transitions(&text()[..1000], 128 * 1024).is_empty());
        let mut mixed = text()[..2000].to_vec();
        mixed.extend_from_slice(&noise(2, 2000));
        assert!(transitions(&mixed, 128 * 1024).is_empty());
    }
}
