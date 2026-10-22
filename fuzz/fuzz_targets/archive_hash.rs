//! XXH3-128, ours against xxHash's own C, on whatever input a mutator invents
//! — whole, and fed in pieces whose sizes come from the input itself, so the
//! streaming buffer's every boundary is reached. And BLAKE3 hashed on several
//! threads in fixed pieces, against `blake3::hash`, with the piece size, batch
//! and thread count taken from the input too.

#![no_main]

use libfuzzer_sys::fuzz_target;
use zalt_archive::xxh3;

fuzz_target!(|data: &[u8]| {
    let reference = zlta_seekable_probe::xxh3::xxh3_128(data);
    assert_eq!(xxh3::canonical(xxh3::hash(data)), reference, "one-shot");

    // Piece sizes from the input's first bytes, cycling; at least one byte.
    let sizes: Vec<usize> = data.iter().take(8).map(|&b| usize::from(b) * 3 + 1).collect();
    let mut s = xxh3::Xxh3::new();
    let mut at = 0;
    let mut k = 0;
    while at < data.len() {
        let n = sizes.get(k % sizes.len().max(1)).copied().unwrap_or(1).min(data.len() - at);
        s.update(&data[at..at + n]);
        at += n;
        k += 1;
    }
    assert_eq!(xxh3::canonical(s.digest()), reference, "streamed");

    let (piece_log, batch, threads) = match data {
        [a, b, c, ..] => (10 + u32::from(a % 3), usize::from(b % 5) + 1, usize::from(c % 4) + 1),
        _ => (10, 1, 1),
    };
    let mut p = zalt_archive::hash::ParallelBlake3::with_pieces(piece_log, batch, threads);
    let mut at = 0;
    let mut k = 0;
    while at < data.len() {
        let n = sizes.get(k % sizes.len().max(1)).copied().unwrap_or(1).min(data.len() - at);
        p.update(&data[at..at + n]);
        at += n;
        k += 1;
    }
    assert_eq!(p.finalize(), blake3::hash(data), "parallel BLAKE3");
});
