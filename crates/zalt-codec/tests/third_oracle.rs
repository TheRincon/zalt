//! Differential decoding against an independent third-party implementation.
//!
//! # Why a second oracle
//!
//! Every conformance check in this repository ultimately asks libzstd. That is
//! the right primary oracle — it is the reference — but it means a *shared
//! misreading of RFC 8878 would be invisible*: if zalt and libzstd happen to
//! agree on a wrong interpretation, nothing here objects, and the golden
//! corpora only cover the cases somebody already thought to capture.
//!
//! `ruzstd` is a pure-Rust zstd decoder written independently of both. It
//! shares no code with libzstd and none with zalt; the only thing all three
//! have in common is the specification. Agreement between two independent
//! readings is therefore evidence about the *spec*, which agreement with the
//! reference alone is not.
//!
//! # The asymmetry, which is different from §9's
//!
//! §9's differential reasoning is about zalt versus libzstd, where "zalt accepts
//! what libzstd rejects" is always a bug. That does not transfer here, because
//! `ruzstd` is neither the reference nor a subset of it — it has its own gaps
//! and its own strictness. So the only assertion worth making is the
//! conditional one:
//!
//! **If `ruzstd` decodes a frame, zalt must decode it to the same bytes.**
//!
//! A frame `ruzstd` refuses says nothing either way and is skipped. That is a
//! weaker property than the libzstd oracle offers, and it is still worth having,
//! because it is checked against a completely separate reading of the format.

use std::io::Read;
use std::path::{Path, PathBuf};

use zalt_codec::params::DecodeLimits;

fn workspace_root() -> PathBuf {
    // tests/ -> zalt-codec/ -> crates/ -> workspace
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the manifest directory exists")
}

fn zst_files_in(dir: PathBuf) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut frames: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "zst"))
        .collect();
    frames.sort();
    frames
}

/// Every frame worth putting in front of a second decoder.
///
/// The golden corpus is four files, which makes a differential test against it
/// close to ceremonial. The frames `just decodecorpus` generates are the real
/// material — thousands of them, spread across the legal format on purpose —
/// so they are included when present. They live under `target/`, so this is
/// opportunistic rather than required: run the recipe and this test gets
/// sharper, skip it and it still checks the golden frames.
fn golden_frames() -> Vec<PathBuf> {
    let root = workspace_root();
    let mut frames = zst_files_in(root.join("zstd-fixtures/golden-decompression"));
    let generated = zst_files_in(root.join("target/decodecorpus/frames"));
    // Cheap in CI, where the generator has not run; the cap keeps a large local
    // corpus from dominating `just t`.
    frames.extend(generated.into_iter().take(400));
    frames
}

/// Decode with the third-party implementation, or `None` if it declines.
fn ruzstd_decode(frame: &[u8]) -> Option<Vec<u8>> {
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(frame).ok()?;
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).ok()?;
    Some(out)
}

#[test]
fn zalt_agrees_with_an_independent_decoder_on_the_golden_corpus() {
    let frames = golden_frames();
    if frames.is_empty() {
        eprintln!("zstd-fixtures/ not present; skipping");
        return;
    }

    let mut compared = 0usize;
    let mut declined = 0usize;

    for path in &frames {
        let bytes = std::fs::read(path).expect("fixture is readable");

        let Some(expected) = ruzstd_decode(&bytes) else {
            // Not a verdict on zalt: this implementation simply did not take it.
            declined += 1;
            continue;
        };

        let ours = zalt_codec::decompress(&bytes, DecodeLimits::default()).unwrap_or_else(|e| {
            panic!(
                "{}: zalt refused a frame ruzstd decoded: {e}",
                path.display()
            )
        });

        assert_eq!(
            ours,
            expected,
            "{}: zalt and ruzstd disagree — two independent readings of RFC 8878 \
             cannot both be right, so this is a real conformance question",
            path.display()
        );
        compared += 1;
    }

    // The count is the point. A differential test that silently compared
    // nothing would pass exactly as loudly as one that compared everything —
    // the failure this repository has already written a recipe around.
    assert!(
        compared > 0,
        "no frames were actually compared ({declined} declined by ruzstd); \
         this test would pass without checking anything"
    );
    eprintln!("compared {compared} frames against ruzstd, {declined} declined");
}
