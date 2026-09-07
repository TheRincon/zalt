//! Dictionary loading against the reference's own dictionary corpus.
//!
//! `zstd-fixtures/golden-dictionaries/` is the set facebook/zstd keeps for this,
//! and §9 marked it "For M4" — this is M4. Like the other golden sets, each file
//! is a trap somebody already fell into: `http-dict-missing-symbols` is a
//! dictionary whose entropy tables do not mention every symbol, which is legal
//! and which a decoder that assumes full coverage will mishandle.

use std::path::{Path, PathBuf};

use zalt_codec::dict::Dictionary;

fn fixture_dir() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../zstd-fixtures/golden-dictionaries");
    dir.is_dir().then_some(dir)
}

#[test]
fn golden_dictionaries_load() {
    let Some(dir) = fixture_dir() else {
        eprintln!("zstd-fixtures/ not present; skipping");
        return;
    };

    let mut checked = 0usize;
    for entry in std::fs::read_dir(&dir).expect("readable").flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let raw = std::fs::read(&path).expect("fixture is readable");
        let dict = Dictionary::parse(&raw)
            .unwrap_or_else(|e| panic!("{}: reference dictionary refused: {e}", path.display()));

        // Every file in this directory is a *formatted* dictionary, so each
        // must carry an identity and pre-built tables. A raw-content reading
        // would also "succeed" and would be silently wrong, which is the
        // failure this assertion exists to prevent.
        assert!(
            dict.has_entropy(),
            "{}: parsed as raw content, so the magic was not recognised",
            path.display()
        );
        assert_ne!(
            dict.id(),
            0,
            "{}: formatted dictionary needs an ID",
            path.display()
        );
        assert!(
            !dict.content().is_empty(),
            "{}: a dictionary with no content cannot seed history",
            path.display()
        );

        let entropy = dict.entropy().expect("checked above");
        for (i, rep) in entropy.repeats.iter().enumerate() {
            assert!(*rep > 0, "{}: repeat offset {i} is zero", path.display());
            assert!(
                *rep as usize <= dict.content().len(),
                "{}: repeat offset {i} reaches past the content",
                path.display()
            );
        }
        checked += 1;
    }

    // The count, for the reason `just decodecorpus` prints one: a loop over an
    // empty directory passes exactly as loudly as one that checked everything.
    assert!(checked > 0, "no dictionaries were actually parsed");
    eprintln!("parsed {checked} golden dictionaries");
}

/// Every truncation of a real dictionary is an error, never a panic.
///
/// The fixture is a genuine formatted dictionary, so cutting it short walks the
/// parser through each of its stages — magic, ID, three FSE tables, the Huffman
/// table, the repeat offsets — with the input ending mid-field.
#[test]
fn truncated_dictionaries_are_refused_not_fatal() {
    let Some(dir) = fixture_dir() else {
        eprintln!("zstd-fixtures/ not present; skipping");
        return;
    };
    let Some(path) = std::fs::read_dir(&dir)
        .expect("readable")
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_file())
    else {
        return;
    };

    let raw = std::fs::read(&path).expect("readable");
    assert!(Dictionary::parse(&raw).is_ok(), "the whole fixture parses");
    for cut in 0..raw.len() {
        // No assertion on the verdict: some prefixes are legal raw-content
        // dictionaries. The property is that none of them is fatal.
        let _ = Dictionary::parse(&raw[..cut]);
    }
}

/// A single flipped byte must never turn into a panic.
#[test]
fn corrupted_dictionaries_are_refused_not_fatal() {
    let Some(dir) = fixture_dir() else {
        return;
    };
    let Some(path) = std::fs::read_dir(&dir)
        .expect("readable")
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_file())
    else {
        return;
    };

    let raw = std::fs::read(&path).expect("readable");
    // Walk the header and table region, where structure lives; the content tail
    // is bytes and cannot be malformed.
    let reach = raw.len().min(512);
    for i in 0..reach {
        for bit in [0x01u8, 0x80] {
            let mut corrupted = raw.clone();
            corrupted[i] ^= bit;
            let _ = Dictionary::parse(&corrupted);
        }
    }
}

/// The wiring, not just the loader: decode frames the reference compressed
/// *with* a dictionary, and match it byte for byte.
///
/// This is the test that would catch a dictionary seeded into the wrong place.
/// Loading a dictionary correctly and then failing to make its content
/// reachable, or seeding entropy tables the first block then overwrites, both
/// leave the loader passing and the decoder wrong — and wrong in the way this
/// codec fears, which is plausible output rather than an error.
#[test]
fn frames_compressed_with_a_dictionary_decode_byte_exactly() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let generated = root.join("zstd-fixtures/generated");
    let dicts = root.join("zstd-fixtures/golden-dictionaries");
    let sources = root.join("zstd-fixtures/golden-compression");
    if !generated.is_dir() || !dicts.is_dir() {
        eprintln!("fixtures not generated; run `just zstd-fixtures`");
        return;
    }

    let mut checked = 0usize;
    for entry in std::fs::read_dir(&generated).expect("readable").flatten() {
        let path = entry.path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        // <source>.D-<dictionary>.L<level>.zst
        let Some((source, tail)) = name.split_once(".D-") else {
            continue;
        };
        let Some((dict_name, _level)) = tail.split_once(".L") else {
            continue;
        };

        let dict_raw = std::fs::read(dicts.join(dict_name)).expect("dictionary is readable");
        let dictionary = Dictionary::parse(&dict_raw).expect("golden dictionary parses");

        let frame = std::fs::read(&path).expect("frame is readable");
        let expected = std::fs::read(sources.join(source)).expect("source is readable");

        let mut decoder = zalt_codec::Decoder::<zalt_codec::Scalar>::new(
            zalt_codec::params::DecodeLimits::default(),
        );
        decoder.load_dictionary(dictionary);

        let mut out = Vec::new();
        decoder
            .decompress(&frame, &mut out)
            .unwrap_or_else(|e| panic!("{name}: {e}"));

        assert_eq!(out, expected, "{name}: decoded output differs");
        checked += 1;
    }

    assert!(
        checked > 0,
        "no dictionary-compressed frames were checked; the fixtures may not have been generated"
    );
    eprintln!("decoded {checked} dictionary-compressed frames");
}
