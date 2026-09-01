//! Conformance against the reference implementation's own regression corpus.
//!
//! These fixtures come from `facebook/zstd`'s test suite, and each one is a
//! trap somebody already fell into. That makes them worth more per byte than
//! anything generated: `rle-first-block` exercises a block type the encoder
//! almost never emits, `zeroSeq_2B` is the zero-sequences edge case where the
//! section ends *before* the symbol-modes byte, and the whole
//! `golden-decompression-errors` directory tests the direction that actually
//! matters for security — that invalid frames are **refused**.
//!
//! # Why the fixtures are read at runtime
//!
//! `include_bytes!` needs a compile-time path, so a fixed list would have to be
//! edited every time the corpus grows. Walking the directory instead means
//! dropping a new file in is enough, and a corpus that is not present simply
//! skips rather than failing the build for someone who has not fetched it.

use std::fs;
use std::path::{Path, PathBuf};

use zalt_codec::params::DecodeLimits;

fn corpus_root() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .parent()?
        .join("zstd-fixtures");
    root.is_dir().then_some(root)
}

/// Permissive bounds. These frames are the reference's own, and some
/// legitimately declare large windows; a browser policy would reject them for
/// reasons that have nothing to do with conformance.
fn limits() -> DecodeLimits {
    DecodeLimits::default()
}

/// Largest frame worth decoding under Miri.
///
/// Miri interprets every operation, so `rle-first-block` — which expands to a
/// megabyte — would take many minutes. The block and sequence paths it
/// exercises are covered by smaller frames; what Miri is here to find does not
/// depend on volume.
#[cfg(miri)]
const MIRI_SIZE_LIMIT: usize = 64 * 1024;

fn too_big_for_miri(_len: usize) -> bool {
    #[cfg(miri)]
    {
        _len > MIRI_SIZE_LIMIT
    }
    #[cfg(not(miri))]
    {
        false
    }
}

fn files_in(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == ext))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Frames the reference decodes must decode here, byte for byte.
#[test]
fn golden_decompression_frames_decode_exactly() {
    let Some(root) = corpus_root() else {
        eprintln!("zstd-fixtures/ not present; skipping");
        return;
    };
    let dir = root.join("golden-decompression");
    let generated = root.join("generated");
    let frames = files_in(&dir, "zst");
    assert!(!frames.is_empty(), "no golden frames found in {dir:?}");

    let mut checked = 0;
    for frame in &frames {
        let stem = frame.file_stem().unwrap().to_string_lossy().to_string();
        let expected_path = generated.join(format!("{stem}.expected"));
        let Ok(expected) = fs::read(&expected_path) else {
            eprintln!("no expected output for {stem}; run `just zstd-fixtures`");
            continue;
        };
        if too_big_for_miri(expected.len()) {
            continue;
        }
        let input = fs::read(frame).unwrap();

        match zalt_codec::decompress(&input, limits()) {
            Ok(got) => assert_eq!(
                got.len(),
                expected.len(),
                "{stem}: decoded {} bytes, expected {}",
                got.len(),
                expected.len()
            ),
            Err(e) => panic!("{stem}: failed to decode a reference golden frame: {e}"),
        }
        // Compared separately so a length mismatch reports a length rather than
        // dumping a megabyte of bytes.
        let got = zalt_codec::decompress(&input, limits()).unwrap();
        assert!(got == expected, "{stem}: decoded content differs");
        checked += 1;
    }
    if !cfg!(miri) {
        assert!(checked > 0, "no golden frames were actually checked");
    }
    eprintln!("checked {checked} golden frames");
}

/// Frames the reference **refuses** must be refused here too.
///
/// This is the direction that matters: accepting a malformed frame is how a
/// decoder's validation gap turns into a reachable memory-safety bug. A
/// decoder that is merely *permissive* passes every round-trip test ever
/// written and is still wrong.
#[test]
fn golden_error_frames_are_rejected() {
    let Some(root) = corpus_root() else {
        eprintln!("zstd-fixtures/ not present; skipping");
        return;
    };
    let dir = root.join("golden-decompression-errors");
    let frames = files_in(&dir, "zst");
    assert!(!frames.is_empty(), "no error frames found in {dir:?}");

    for frame in &frames {
        let name = frame.file_name().unwrap().to_string_lossy().to_string();
        let input = fs::read(frame).unwrap();
        match zalt_codec::decompress(&input, limits()) {
            Err(_) => {}
            Ok(out) => panic!(
                "{name}: accepted a frame the reference rejects, producing {} bytes",
                out.len()
            ),
        }
    }
    eprintln!("rejected {} malformed frames", frames.len());
}

/// Round-trip the reference's compression corpus at several levels.
///
/// These inputs are chosen to stress specific encoder paths, which means they
/// stress the corresponding decoder paths: `large-literal-and-match-lengths`
/// reaches the high literals-length and match-length codes with their large
/// baselines and wide extra-bit fields, and `huffman-compressed-larger` covers
/// the case where Huffman coding does not pay.
#[test]
fn golden_compression_corpus_round_trips() {
    let Some(root) = corpus_root() else {
        eprintln!("zstd-fixtures/ not present; skipping");
        return;
    };
    let sources = root.join("golden-compression");
    let generated = root.join("generated");
    let Ok(entries) = fs::read_dir(&sources) else {
        eprintln!("no golden-compression directory; skipping");
        return;
    };

    let mut checked = 0;
    let mut names: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    names.sort();

    for source in names {
        if !source.is_file() {
            continue;
        }
        let name = source.file_name().unwrap().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let Ok(expected) = fs::read(&source) else {
            continue;
        };

        for level in [1, 3, 9, 19] {
            let frame_path = generated.join(format!("{name}.L{level}.zst"));
            let Ok(frame) = fs::read(&frame_path) else {
                continue;
            };
            if too_big_for_miri(expected.len()) {
                continue;
            }

            match zalt_codec::decompress(&frame, limits()) {
                Ok(got) => {
                    assert_eq!(
                        got.len(),
                        expected.len(),
                        "{name} L{level}: length mismatch"
                    );
                    assert!(got == expected, "{name} L{level}: content mismatch");
                }
                Err(e) => panic!("{name} L{level}: failed to decode: {e}"),
            }
            checked += 1;
        }
    }
    if !cfg!(miri) {
        assert!(checked > 0, "no compression fixtures were checked");
    }
    eprintln!("round-tripped {checked} frames");
}

/// Frames a differential fuzz campaign found zalt accepting and libzstd
/// refusing — the asymmetry §9 calls "always a bug, a hole in validation".
///
/// All three carry a Huffman literals stream with more readable bits than its
/// symbols consume: seven literals of at most two bits each against twenty
/// bits. The decoder produced the right seven bytes and reported success,
/// because a backward reader returns zeros past the end rather than failing,
/// and nothing required the stream to end where it claimed to.
///
/// Kept as bytes rather than as files under `fuzz/artifacts/`, which is a
/// scratch directory `just clean-fuzz` deletes.
#[test]
fn literal_streams_with_unconsumed_bits_are_refused() {
    const FOUND_BY_FUZZING: [&[u8]; 3] = [
        &[
            0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x00, 0x55, 0x00, 0x00, 0x72, 0x80, 0x01, 0x82, 0x04,
            0x3e, 0x00, 0x1f, 0x13, 0x00,
        ],
        &[
            0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x00, 0x55, 0x00, 0x00, 0x72, 0x80, 0x01, 0x82, 0x20,
            0x04, 0x7e, 0x13, 0x1f, 0x00,
        ],
        &[
            0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x00, 0x55, 0x00, 0x00, 0x72, 0x80, 0x01, 0x82, 0x60,
            0x7e, 0x00, 0x1f, 0x13, 0x00,
        ],
    ];

    // Same campaign, a different gap: a Huffman table declaring `table_log` 2
    // for a code that is only one bit deep.
    const OVERSIZED_TABLE: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x00, 0x55, 0x00, 0x00, 0x72, 0x80, 0x01, 0x04, 0xe0, 0x0d,
        0xff, 0x04, 0xa9, 0x00,
    ];

    for (i, frame) in FOUND_BY_FUZZING
        .iter()
        .copied()
        .chain([OVERSIZED_TABLE])
        .enumerate()
    {
        let result = zalt_codec::decompress(frame, limits());
        assert!(
            result.is_err(),
            "frame {i} decoded to {:?}; libzstd refuses it and so must we",
            result.map(|v| v.len())
        );
    }
}
