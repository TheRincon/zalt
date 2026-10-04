//! Deterministic test corpora.

use crate::tar::{TarEntry, TarWriter};

pub struct Corpus {
    /// Tar body: every entry, no end-of-archive marker.
    pub body: Vec<u8>,
    pub entries: Vec<TarEntry>,
    /// Regular files and their contents, for content checks.
    pub files: Vec<(String, Vec<u8>)>,
}

/// xorshift64* – small, deterministic, good enough for test data.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self((seed ^ 0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }
}

const WORDS: &[&str] = &[
    "archive",
    "frame",
    "seek",
    "table",
    "index",
    "zstd",
    "block",
    "window",
    "offset",
    "entry",
    "apple",
    "silicon",
    "neon",
    "vector",
    "stream",
    "header",
    "footer",
    "magic",
    "checksum",
    "literal",
    "sequence",
    "match",
    "huffman",
    "entropy",
    "dictionary",
    "tar",
    "pax",
    "ustar",
    "varint",
    "record",
    "terminator",
    "skippable",
];

fn text(rng: &mut Rng, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len + 16);
    while v.len() < len {
        v.extend_from_slice(WORDS[rng.below(WORDS.len() as u64) as usize].as_bytes());
        v.push(if rng.below(12) == 0 { b'\n' } else { b' ' });
    }
    v.truncate(len);
    v
}

/// Semi-compressible: interleaves text runs with random bytes.
fn noisy(rng: &mut Rng, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len + 64);
    while v.len() < len {
        if rng.below(3) == 0 {
            v.extend((0..64).map(|_| rng.next_u64() as u8));
        } else {
            v.extend(text(rng, 64));
        }
    }
    v.truncate(len);
    v
}

/// About 5 MiB of mixed content. It includes directories, many small files that
/// share solid frames, medium files, a 3 MiB file that gets split across frames,
/// an empty file, a symlink, a >100-byte path and a non-ASCII path (both need pax
/// headers, so `Header_Offset != data_offset - 512`).
pub fn standard(seed: u64) -> Corpus {
    let mut rng = Rng::new(seed);
    let mut w = TarWriter::new();
    let mut files = Vec::new();
    let mtime = 1_790_000_000;
    let mut file = |w: &mut TarWriter, path: String, data: Vec<u8>| {
        w.add_file(&path, &data, 0o644, mtime);
        files.push((path, data));
    };

    w.add_dir("proj", 0o755, mtime);
    w.add_dir("proj/src", 0o755, mtime);
    for i in 0..60 {
        let len = 200 + rng.below(20_000) as usize;
        let d = text(&mut rng, len);
        file(&mut w, format!("proj/src/file_{i:03}.txt"), d);
    }
    w.add_dir("proj/assets", 0o755, mtime);
    for i in 0..5 {
        let len = 100_000 + rng.below(200_000) as usize;
        let d = noisy(&mut rng, len);
        file(&mut w, format!("proj/assets/medium_{i}.bin"), d);
    }
    let big = noisy(&mut rng, 3 * 1024 * 1024 + 777);
    file(&mut w, "proj/assets/big.bin".into(), big);
    file(&mut w, "proj/empty.txt".into(), Vec::new());
    w.add_symlink("proj/latest", "assets/big.bin", mtime);
    let long = format!(
        "proj/{}file_with_a_rather_long_name.txt",
        "deeply_nested_directory_name/".repeat(4)
    );
    assert!(long.len() > 100);
    let d = text(&mut rng, 4000);
    file(&mut w, long, d);
    let d = text(&mut rng, 3000);
    file(&mut w, "proj/données/résumé.txt".into(), d);

    let (body, entries) = w.finish();
    Corpus {
        body,
        entries,
        files,
    }
}

/// `n` tiny files. With a 512-byte chunk size, each entry becomes two frames, which
/// makes a very large seek table.
pub fn many_tiny(n: usize, seed: u64) -> Corpus {
    let mut rng = Rng::new(seed);
    let mut w = TarWriter::new();
    let mut files = Vec::new();
    for i in 0..n {
        let path = format!("tiny/{:02}/f{i:06}", i % 97);
        let len = 1 + rng.below(511) as usize;
        let d = text(&mut rng, len);
        w.add_file(&path, &d, 0o644, 1_790_000_000);
        files.push((path, d));
    }
    let (body, entries) = w.finish();
    Corpus {
        body,
        entries,
        files,
    }
}

/// No entries at all: the archive is just header, Terminator, index and seek table.
pub fn empty() -> Corpus {
    Corpus {
        body: Vec::new(),
        entries: Vec::new(),
        files: Vec::new(),
    }
}
