//! The ZTML recipe: that its two copies agree, that its dictionary is what the
//! provenance says, and that a frame compressed against it decodes byte-exactly.

use std::path::{Path, PathBuf};
use std::process::Command;

use zalt_recipe::recipe::{DictionarySource, Strategy};

/// The manifest text and the Rust value must describe the same recipe.
///
/// This is the test the whole two-copies arrangement is predicated on. A host
/// baking the recipe into a binary uses [`zalt_ztml::recipe`] and never parses
/// anything; the CLI loads the `.toml`. If they drift, one of those two hosts
/// silently gets different tuning, and nothing else in the suite would notice —
/// a recipe cannot produce wrong bytes, only a worse ratio, which is invisible
/// without a comparison nobody runs.
#[test]
fn the_manifest_and_the_rust_value_describe_the_same_recipe() {
    let parsed = zalt_recipe::parse(zalt_ztml::MANIFEST).expect("the shipped manifest must parse");
    assert_eq!(parsed, zalt_ztml::recipe());
}

#[test]
fn the_recipe_claims_ztml_and_nothing_else() {
    let r = zalt_ztml::recipe();
    assert_eq!(r.name, zalt_ztml::NAME);
    assert!(r.claims(Some("application/ztml"), None));
    assert!(r.claims(Some("application/ztml; charset=utf-8"), None));
    assert!(r.claims(None, Some("paper.zdoc")));
    assert!(!r.claims(Some("text/html"), Some("page.html")));
}

#[test]
fn already_compressed_media_is_not_compressed_again() {
    let r = zalt_ztml::recipe();
    // The §8 rule: zstd over these yields about 1.0x for a full encode's cost.
    for ct in ["image/webp", "image/png", "image/avif", "font/woff2"] {
        assert_eq!(
            r.encode_for(Some(ct), None).strategy,
            Strategy::Stored,
            "{ct} should not be compressed"
        );
    }
    // Reached by filename too, for a subresource served without a content type.
    assert_eq!(
        r.encode_for(None, Some("photo.WEBP")).strategy,
        Strategy::Stored
    );
    // And the document itself is still compressed, at the manifest's level.
    let doc = r.encode_for(Some("application/ztml"), None);
    assert_eq!(doc.strategy, Strategy::Compress);
    // Level 9, from `just ztml-sweep`: the knee of the ratio/encode-cost
    // curve. See the note in data/ztml.toml.
    assert_eq!(doc.level, Some(9));
}

#[test]
fn the_recipe_can_only_tighten_a_host_policy() {
    use zalt_codec::params::DecodeLimits;
    // The recipe asks for an 8 MiB window. A host already stricter than that
    // must keep its own number — the property `zalt-recipe` is built around,
    // checked here on the recipe that actually ships.
    let strict = DecodeLimits {
        max_window_log: 20,
        max_decoded_size: Some(4096),
    };
    let out = zalt_ztml::recipe().decode.narrow(strict);
    assert_eq!(out.max_window_log, 20);
    assert_eq!(out.max_decoded_size, Some(4096));
}

#[test]
fn the_embedded_dictionary_parses_as_raw_content() {
    let entry = zalt_ztml::entry().expect("the embedded dictionary must parse");
    let dict = entry.dictionary.expect("a dictionary is embedded");

    // Raw content, so no pre-built tables and no identity. Both halves matter:
    // `has_entropy` false is what makes the Dictionary_ID 0 legitimate rather
    // than a corrupt formatted dictionary that happened to parse.
    assert!(!dict.has_entropy(), "v1 is a content prefix, not formatted");
    assert_eq!(dict.id(), 0, "a raw-content dictionary carries no id");
    assert_eq!(dict.content(), zalt_ztml::DICTIONARY);

    // The manifest must not claim an id the dictionary cannot carry. If it did,
    // `zalt-recipe`'s load path would reject the pair — correctly, and only at
    // runtime on whoever loaded it from disk.
    let reference = zalt_ztml::recipe()
        .dictionary
        .expect("a dictionary is named");
    assert_eq!(reference.id, None);
    assert_eq!(
        reference.source,
        DictionarySource::Path("ztml-0.3.dict".into())
    );
}

#[test]
fn the_dictionary_matches_its_recorded_provenance() {
    // The blob is checked in; the provenance file records what produced it.
    // A dictionary whose bytes no longer match its inputs is one nobody can
    // regenerate, which makes every ratio measured against it unreproducible.
    let recorded = zalt_ztml::DICTIONARY_PROVENANCE
        .lines()
        .find_map(|l| l.strip_prefix("bytes"))
        .expect("provenance records a length")
        .trim()
        .parse::<usize>()
        .expect("a number");
    assert_eq!(recorded, zalt_ztml::DICTIONARY.len());

    // The vocabulary leads and the samples follow: end-weighted, because zstd
    // matches backwards from the dictionary's end.
    let order: Vec<&str> = zalt_ztml::DICTIONARY_PROVENANCE
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| l.rsplit("  ").next().filter(|s| s.contains(':')))
        .collect();
    assert!(
        order.first().is_some_and(|s| s.starts_with("vocabulary:")),
        "the vocabulary block leads: {order:?}"
    );
    assert!(
        order.iter().skip(1).all(|s| s.starts_with("sample:")),
        "samples follow the vocabulary: {order:?}"
    );
}

// ------------------------------------------------- end to end, against zstd

fn corpus_dir() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../doc");
    dir.is_dir().then_some(dir)
}

fn zstd_binary() -> Option<String> {
    let zstd = std::env::var("ZSTD").unwrap_or_else(|_| "zstd".into());
    Command::new(&zstd)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| zstd)
}

/// Compress real documents against the shipped dictionary and decode them back.
///
/// The reference compresses, zalt decodes. That direction is the only one
/// available until M5, and it is also the one that matters: a dictionary that
/// seeds the wrong history produces plausible-looking output rather than an
/// error, so byte-exactness against the reference is the only real check.
#[test]
fn documents_compressed_against_the_shipped_dictionary_decode_byte_exactly() {
    let (Some(dir), Some(zstd)) = (corpus_dir(), zstd_binary()) else {
        eprintln!("corpus or zstd not present; skipping");
        return;
    };

    let dict_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("data/ztml-0.3.dict");
    let dictionary =
        zalt_codec::dict::Dictionary::parse(zalt_ztml::DICTIONARY).expect("dictionary parses");

    let mut checked = 0usize;
    for entry in std::fs::read_dir(&dir).expect("readable").flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "zdoc") {
            continue;
        }
        let expected = std::fs::read(&path).expect("readable");

        for level in ["3", "6", "12"] {
            let out = Command::new(&zstd)
                .args(["-q", "-c", &format!("-{level}"), "-D"])
                .arg(&dict_path)
                .arg(&path)
                .output()
                .expect("zstd runs");
            assert!(out.status.success(), "zstd failed on {}", path.display());

            let mut decoder = zalt_codec::Decoder::<zalt_codec::Scalar>::new(
                zalt_codec::params::DecodeLimits::default(),
            );
            decoder.load_dictionary(dictionary.clone());

            let mut got = Vec::new();
            decoder
                .decompress(&out.stdout, &mut got)
                .unwrap_or_else(|e| panic!("{} at L{level}: {e}", path.display()));
            assert_eq!(
                got,
                expected,
                "{} at L{level}: decoded output differs",
                path.display()
            );
            checked += 1;
        }
    }

    // A loop over a corpus nobody generated passes exactly as quietly as one
    // that verified something.
    assert!(checked > 0, "no .zdoc documents were checked");
    eprintln!("checked {checked} dictionary-compressed frames");
}
