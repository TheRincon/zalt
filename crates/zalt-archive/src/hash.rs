//! Entry hashes (§7.3): which algorithm an archive uses, and computing one.
//!
//! **BLAKE3 is the default** — decided 2026-10 for the protection a
//! cryptographic hash keeps open: content deduplication (Q4) and a signed
//! index (Q3) are only sound over a hash nobody can collide on purpose, and
//! an archive written unhashed or with XXH3 would have to be re-hashed before
//! either could apply. XXH3-128 stays available for archives that only need
//! to detect accidents, at a fraction of the cost. `doc/ZALT-MAC.md` M7
//! records the decision and the measurements.
//!
//! BLAKE3 is the authors' `blake3` crate with its `pure` feature: on aarch64
//! that builds no C and runs no `unsafe`, only its portable Rust. XXH3 is
//! [`crate::xxh3`], ours. SHA-256 (algorithm 3) is readable — its digests
//! parse — but not computed, so it is neither written nor checked.

/// `Hash_Algorithm` values (§7.3).
pub mod algorithm {
    pub const NONE: u8 = 0;
    pub const XXH3_128: u8 = 1;
    pub const BLAKE3: u8 = 2;
    pub const SHA256: u8 = 3;
}

/// The hash a writer gives each member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Hash {
    /// No entry hashes. Frames still carry their checksums.
    None,
    /// XXH3-128: catches accidental damage, nothing deliberate.
    Xxh3,
    /// BLAKE3-256: the default — see the module comment.
    #[default]
    Blake3,
}

impl Hash {
    /// The `Hash_Algorithm` value it writes.
    pub fn algorithm(self) -> u8 {
        match self {
            Hash::None => algorithm::NONE,
            Hash::Xxh3 => algorithm::XXH3_128,
            Hash::Blake3 => algorithm::BLAKE3,
        }
    }

    /// The name, as the CLI spells it.
    pub fn name(self) -> &'static str {
        match self {
            Hash::None => "none",
            Hash::Xxh3 => "xxh3",
            Hash::Blake3 => "blake3",
        }
    }
}

/// The algorithm's name, for messages.
pub fn algorithm_name(algorithm: u8) -> &'static str {
    match algorithm {
        algorithm::NONE => "none",
        algorithm::XXH3_128 => "XXH3-128",
        algorithm::BLAKE3 => "BLAKE3",
        algorithm::SHA256 => "SHA-256",
        _ => "unknown",
    }
}

/// A member's hash, computed as its content goes past.
pub(crate) enum Hasher {
    Xxh3(Box<crate::xxh3::Xxh3>),
    Blake3(Box<ParallelBlake3>),
}

impl Hasher {
    /// A hasher for `algorithm`, or `None` where there is nothing to compute:
    /// no hashing, or one this crate does not implement. `threads` is how
    /// many BLAKE3 may use; XXH3 is single-threaded and fast enough to be.
    pub(crate) fn new(algorithm: u8, threads: usize) -> Option<Self> {
        match algorithm {
            algorithm::XXH3_128 => Some(Hasher::Xxh3(Box::default())),
            algorithm::BLAKE3 => Some(Hasher::Blake3(Box::new(ParallelBlake3::new(threads)))),
            _ => None,
        }
    }

    pub(crate) fn update(&mut self, bytes: &[u8]) {
        match self {
            Hasher::Xxh3(h) => h.update(bytes),
            Hasher::Blake3(h) => h.update(bytes),
        }
    }

    /// The digest, as §7.2's raw bytes.
    pub(crate) fn finish(&self) -> Vec<u8> {
        match self {
            Hasher::Xxh3(h) => crate::xxh3::canonical(h.digest()).to_vec(),
            Hasher::Blake3(h) => h.finalize().as_bytes().to_vec(),
        }
    }
}

/// Whether this crate can check digests of `algorithm`.
pub fn can_check(algorithm: u8) -> bool {
    matches!(algorithm, algorithm::XXH3_128 | algorithm::BLAKE3)
}

/// BLAKE3 over input that arrives in pieces, hashed on several threads —
/// the same digest as `blake3::hash` on the whole, whatever the thread count.
///
/// **Why.** The `pure` build hashes about 500 MB/s on one core, slower than
/// zalt decodes: measured 2026-10-14, BLAKE3 made `verify` 2.1x and `extract`
/// 1.85x the time of no hash at all, where everything around it ran on every
/// core.
///
/// **How.** BLAKE3 is a tree: 1 KiB chunks at the leaves, and every subtree
/// of a power-of-two number of chunks, aligned to its size, is a node whose
/// chaining value can be computed alone — with `set_input_offset` saying
/// where it starts — and merged with its neighbour's. So the input is cut
/// into fixed pieces of `2^piece_log` bytes, the last possibly short; batches
/// of pieces are hashed in parallel by the `blake3` crate itself; and the
/// chaining values are merged into the root at the end, splitting each range
/// where BLAKE3's own tree does (`left_subtree_len`). That is the recipe the
/// crate's `hazmat` documentation gives for this, and the only parts that
/// are ours are the cutting and the merging order — which is what the tests
/// hold against `blake3::hash` on every tree shape, with pieces small enough
/// to reach them all.
///
/// A piece is only finalized once input follows it: a piece that turned out
/// to be the whole input would be the root, and is hashed differently. An
/// input of one piece or less is just `blake3::hash`.
pub struct ParallelBlake3 {
    piece_log: u32,
    /// Pieces hashed per batch, across `threads`.
    batch: usize,
    threads: usize,
    /// Input not yet hashed: less than a batch, or the end of the input.
    pending: Vec<u8>,
    /// Chaining values of the pieces hashed so far, in order.
    cvs: Vec<blake3::hazmat::ChainingValue>,
    /// Bytes the chaining values cover.
    hashed: u64,
}

impl ParallelBlake3 {
    /// 256 KiB pieces, four per thread per batch.
    pub fn new(threads: usize) -> Self {
        Self::with_pieces(18, 4 * threads.max(1), threads)
    }

    /// Pieces of `2^piece_log` bytes (at least one 1 KiB chunk), `batch` of
    /// them hashed at a time on `threads` threads. For tests: small pieces
    /// reach every shape of tree with little input.
    pub fn with_pieces(piece_log: u32, batch: usize, threads: usize) -> Self {
        assert!(piece_log >= 10, "a piece is at least one BLAKE3 chunk");
        Self {
            piece_log,
            batch: batch.max(1),
            threads: threads.max(1),
            pending: Vec::new(),
            cvs: Vec::new(),
            hashed: 0,
        }
    }

    fn piece(&self) -> usize {
        1 << self.piece_log
    }

    pub fn update(&mut self, input: &[u8]) {
        self.pending.extend_from_slice(input);
        let piece = self.piece();
        // Strictly more than a batch, so the pieces hashed are never the
        // input's last: at least one byte stays behind them.
        if self.pending.len() > self.batch * piece {
            let full = (self.pending.len() - 1) / piece * piece;
            let cvs = hash_pieces(&self.pending[..full], self.hashed, piece, self.threads);
            self.cvs.extend(cvs);
            self.hashed += full as u64;
            self.pending.drain(..full);
        }
    }

    pub fn finalize(&self) -> blake3::Hash {
        let total = self.hashed + self.pending.len() as u64;
        let piece = self.piece();
        if total <= piece as u64 {
            // One piece or less: no piece is merged, and the input may be
            // shorter than a chunk, which only `blake3::hash` can take.
            return blake3::hash(&self.pending);
        }
        let mut cvs = self.cvs.clone();
        cvs.extend(hash_pieces(&self.pending, self.hashed, piece, self.threads));
        let left = blake3::hazmat::left_subtree_len(total);
        let k = (left >> self.piece_log) as usize;
        blake3::hazmat::merge_subtrees_root(
            &subtree(&cvs[..k], left, self.piece_log),
            &subtree(&cvs[k..], total - left, self.piece_log),
            blake3::hazmat::Mode::Hash,
        )
    }
}

/// The chaining value of the subtree whose pieces are `cvs`, covering `len`
/// bytes: split where BLAKE3 splits, merge each side, join them.
fn subtree(
    cvs: &[blake3::hazmat::ChainingValue],
    len: u64,
    piece_log: u32,
) -> blake3::hazmat::ChainingValue {
    if cvs.len() == 1 {
        return cvs[0];
    }
    // More than one piece, so more than one chunk: `left_subtree_len` is a
    // power of two no smaller than a piece, and so on a piece boundary.
    let left = blake3::hazmat::left_subtree_len(len);
    let k = (left >> piece_log) as usize;
    blake3::hazmat::merge_subtrees_non_root(
        &subtree(&cvs[..k], left, piece_log),
        &subtree(&cvs[k..], len - left, piece_log),
        blake3::hazmat::Mode::Hash,
    )
}

/// The non-root chaining values of `data`'s pieces, `data` starting at input
/// offset `offset`, a multiple of `piece`. Every piece is whole but the last.
fn hash_pieces(
    data: &[u8],
    offset: u64,
    piece: usize,
    threads: usize,
) -> Vec<blake3::hazmat::ChainingValue> {
    use blake3::hazmat::HasherExt;
    let one = |i: usize, p: &[u8]| {
        blake3::Hasher::new()
            .set_input_offset(offset + (i * piece) as u64)
            .update(p)
            .finalize_non_root()
    };
    let pieces: Vec<&[u8]> = data.chunks(piece).collect();
    if threads <= 1 || pieces.len() <= 1 {
        return pieces.iter().enumerate().map(|(i, p)| one(i, p)).collect();
    }
    // Contiguous runs of pieces per thread, results in order.
    let per = pieces.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let handles: Vec<_> = pieces
            .chunks(per)
            .enumerate()
            .map(|(t, run)| {
                let one = &one;
                scope.spawn(move || {
                    run.iter()
                        .enumerate()
                        .map(|(j, p)| one(t * per + j, p))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(n: usize) -> Vec<u8> {
        let mut s = 0x2545_F491u32;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                s as u8
            })
            .collect()
    }

    fn parallel(
        input: &[u8],
        piece_log: u32,
        batch: usize,
        threads: usize,
        feed: usize,
    ) -> blake3::Hash {
        let mut h = ParallelBlake3::with_pieces(piece_log, batch, threads);
        for part in input.chunks(feed.max(1)) {
            h.update(part);
        }
        if input.is_empty() {
            h.update(&[]);
        }
        h.finalize()
    }

    /// One-chunk pieces make every tree shape cheap to reach: each length to
    /// 3 KiB, then every chunk boundary and its neighbours to 70 chunks —
    /// left-balanced trees of every size, whole and with a short last chunk.
    /// Whatever the batch, threads and feeding, the digest is `blake3::hash`.
    #[test]
    fn every_tree_shape_gives_blake3s_own_digest() {
        let input = data(72 * 1024);
        let mut lens: Vec<usize> = (0..=3 * 1024).collect();
        for chunks in 3..=70 {
            lens.extend([chunks * 1024 - 1, chunks * 1024, chunks * 1024 + 1]);
        }
        for len in lens {
            let want = blake3::hash(&input[..len]);
            for (batch, threads, feed) in [(1, 1, 4096), (2, 3, 1000), (5, 2, 1), (3, 7, 777)] {
                if feed == 1 && len > 4096 {
                    continue;
                }
                assert_eq!(
                    parallel(&input[..len], 10, batch, threads, feed),
                    want,
                    "len {len}, batch {batch}, threads {threads}, fed {feed} at a time"
                );
            }
        }
    }

    /// Larger pieces, where a piece spans several chunks and the tree's
    /// splits fall inside what one thread hashes.
    #[test]
    fn larger_pieces_give_blake3s_own_digest() {
        let input = data(300 * 1024);
        for piece_log in [11, 12, 14] {
            let piece = 1usize << piece_log;
            for len in [
                piece - 1,
                piece,
                piece + 1,
                2 * piece,
                3 * piece + 5,
                300 * 1024,
            ] {
                let len = len.min(input.len());
                let want = blake3::hash(&input[..len]);
                for (batch, threads) in [(1, 1), (2, 4), (9, 3)] {
                    assert_eq!(
                        parallel(&input[..len], piece_log, batch, threads, 10_000),
                        want,
                        "piece 2^{piece_log}, len {len}, batch {batch}, threads {threads}"
                    );
                }
            }
        }
    }

    /// The geometry the archive uses — 256 KiB pieces, four per thread per
    /// batch — around a piece and around a batch, on eight threads.
    #[test]
    fn the_production_geometry_gives_blake3s_own_digest() {
        let piece = 256 << 10;
        let batch = 4 * 8 * piece;
        let input = data(batch + 3 * piece + 7);
        for len in [
            piece - 1,
            piece,
            piece + 1,
            batch - 1,
            batch,
            batch + 1,
            input.len(),
        ] {
            let mut h = ParallelBlake3::new(8);
            for part in input[..len].chunks(1 << 20) {
                h.update(part);
            }
            assert_eq!(h.finalize(), blake3::hash(&input[..len]), "len {len}");
        }
    }
}
