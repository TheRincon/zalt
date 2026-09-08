//! Manifest conformance: the documented sketch parses, and everything the
//! parser refuses is refused for the stated reason at the stated line.
//!
//! The rejection table is the more valuable half. A parser is easy to test by
//! feeding it correct input, and that tests almost nothing: the failure mode
//! this crate is built against is the manifest that loads *successfully* and
//! tunes nothing, so the assertions that matter are the ones proving specific
//! malformed input does not get through.

use std::fs;
use std::path::PathBuf;

use zalt_recipe::error::ErrorKind;
use zalt_recipe::recipe::{DictionarySource, Strategy};
use zalt_recipe::{RecipeSet, parse};

/// The manifest exactly as `doc/ZALT-MAC.md` §7 prints it.
///
/// Copied verbatim rather than adapted. If the document and the parser drift
/// apart, this test is where it shows up, which is the only reason a sketch in
/// prose is worth anything.
///
/// It names a content class, which `src/` in this crate may not — and that is
/// the intended boundary, not a gap in it. `ci/layering.sh` greps `src/` and
/// not `tests/` on purpose: the rule is about *behaviour* that branches on
/// content, and `zalt-codec/tests/fixtures/text.bin` has carried WEFT-shaped
/// markup as corpus data since M1 on the same reasoning. A fixture that
/// mentions a content class proves nothing about the code; a `match` arm on one
/// proves everything.
const SKETCH: &str = r#"
[recipe]
name = "ztml-0.3"

[match]
content_type = ["application/ztml"]
extension    = [".zdoc"]

[dictionary]
path = "ztml-0.3.dict"
id   = 0x5A544D4C          # carried in Dictionary_ID; both peers resolve it

[encode]
level = 6
window_log = 17

[decode]
max_window = "8MiB"

[[rule]]                    # content already compressed: don't bother
match_content_type = ["image/webp", "image/png", "image/avif", "font/woff2"]
strategy = "stored"
"#;

#[test]
fn the_documented_sketch_parses_field_for_field() {
    let recipe = parse(SKETCH).expect("the sketch in §7 must parse");

    assert_eq!(recipe.name, "ztml-0.3");
    assert_eq!(recipe.match_.content_types, ["application/ztml"]);
    assert_eq!(recipe.match_.extensions, [".zdoc"]);

    let dictionary = recipe.dictionary.as_ref().expect("a dictionary is named");
    assert_eq!(dictionary.id, Some(0x5A54_4D4C));
    assert_eq!(
        dictionary.source,
        DictionarySource::Path("ztml-0.3.dict".into())
    );

    assert_eq!(recipe.encode.level, Some(6));
    assert_eq!(recipe.encode.window_log, Some(17));
    assert_eq!(recipe.encode.strategy, Strategy::Compress);

    // `max_window = "8MiB"` is 2^23.
    assert_eq!(recipe.decode.max_window_log, Some(23));
    assert_eq!(recipe.decode.max_decoded_size, None);

    assert_eq!(recipe.rules.len(), 1);
    let rule = &recipe.rules[0];
    assert_eq!(rule.strategy, Some(Strategy::Stored));
    assert_eq!(
        rule.match_.content_types,
        ["image/webp", "image/png", "image/avif", "font/woff2"]
    );

    // And the rule actually fires, which is the point of having written it.
    assert_eq!(
        recipe.encode_for(Some("image/webp"), None).strategy,
        Strategy::Stored
    );
    assert_eq!(
        recipe.encode_for(Some("application/ztml"), None).strategy,
        Strategy::Compress
    );
}

#[test]
fn accepted_syntax_that_is_easy_to_get_wrong() {
    // Arrays spanning lines, with and without a trailing comma; comments in the
    // middle of one; a `#` inside a string; hex and underscored integers.
    let text = r##"
[recipe]
name = "spans"           # trailing comment

[match]
content_type = [
    "application/a",     # a comment inside an array
    "application/b",
    "application/c"
]
extension = [
    ".a",
    ".b",
]

[dictionary]
path = "dir/has#hash.dict"
id = 0x5A_54_4D_4C

[decode]
max_decoded = 1_048_576
total_memory = "256MiB"
"##;
    let recipe = parse(text).expect("must parse");
    assert_eq!(
        recipe.match_.content_types,
        ["application/a", "application/b", "application/c"]
    );
    assert_eq!(recipe.match_.extensions, [".a", ".b"]);
    assert_eq!(
        recipe.dictionary.as_ref().unwrap().source,
        DictionarySource::Path("dir/has#hash.dict".into()),
        "a # inside a string is part of the value, not the start of a comment"
    );
    assert_eq!(recipe.dictionary.as_ref().unwrap().id, Some(0x5A54_4D4C));
    assert_eq!(recipe.decode.max_decoded_size, Some(1 << 20));
    assert_eq!(recipe.decode.total_memory, Some(256 << 20));
}

#[test]
fn values_are_normalised_on_the_way_in() {
    let text = r#"
[recipe]
name = "normalising"

[match]
content_type = ["Application/ZTML; charset=utf-8"]
extension = ["ZDOC"]
"#;
    let recipe = parse(text).expect("must parse");
    // Normalising at parse time rather than at comparison time means a routing
    // decision never depends on how the manifest happened to be typed.
    assert_eq!(recipe.match_.content_types, ["application/ztml"]);
    assert_eq!(recipe.match_.extensions, [".zdoc"]);
    assert!(recipe.claims(Some("application/ztml"), None));
    assert!(recipe.claims(None, Some("paper.ZDoc")));
}

/// Every rejection, with the line it must be reported at.
///
/// The line number is asserted, not just the kind. A parser that refuses
/// correctly but points at the wrong line sends its reader to the wrong part of
/// the file, and for a config format that is most of the cost of the error.
#[test]
fn malformed_manifests_are_refused_with_the_right_reason() {
    // (label, manifest, expected kind, expected line — 0 means the whole file)
    let cases: &[(&str, &str, ErrorKind, u32)] = &[
        (
            "a key before any table header",
            "name = \"x\"\n",
            ErrorKind::UnknownTable,
            1,
        ),
        (
            "an unknown table",
            "[recipe]\nname = \"x\"\n[encoder]\nlevel = 3\n",
            ErrorKind::UnknownTable,
            3,
        ),
        (
            "an unknown key in a known table",
            "[recipe]\nname = \"x\"\n[encode]\nlevl = 3\n",
            ErrorKind::UnknownKey,
            4,
        ),
        (
            "a repeated key",
            "[recipe]\nname = \"x\"\n[encode]\nlevel = 3\nlevel = 4\n",
            ErrorKind::Duplicate,
            5,
        ),
        (
            "a repeated table",
            "[recipe]\nname = \"x\"\n[encode]\nlevel = 3\n[encode]\n",
            ErrorKind::Duplicate,
            5,
        ),
        (
            "both spellings of the window ceiling",
            "[recipe]\nname = \"x\"\n[decode]\nmax_window = \"8MiB\"\nmax_window_log = 20\n",
            ErrorKind::Duplicate,
            5,
        ),
        (
            "a single string where a list belongs",
            "[recipe]\nname = \"x\"\n[match]\ncontent_type = \"text/plain\"\n",
            ErrorKind::TypeMismatch,
            4,
        ),
        (
            "an array of integers",
            "[recipe]\nname = \"x\"\n[match]\ncontent_type = [1, 2]\n",
            ErrorKind::TypeMismatch,
            4,
        ),
        (
            "a boolean where a string belongs",
            "[recipe]\nname = \"x\"\n[encode]\nstrategy = true\n",
            ErrorKind::TypeMismatch,
            4,
        ),
        (
            "an unknown strategy",
            "[recipe]\nname = \"x\"\n[encode]\nstrategy = \"squeeze\"\n",
            ErrorKind::OutOfRange,
            4,
        ),
        (
            "a window log below the format minimum",
            "[recipe]\nname = \"x\"\n[encode]\nwindow_log = 9\n",
            ErrorKind::OutOfRange,
            4,
        ),
        (
            "a window log above what the implementation accepts",
            "[recipe]\nname = \"x\"\n[decode]\nmax_window_log = 32\n",
            ErrorKind::OutOfRange,
            4,
        ),
        (
            "a compression level past the maximum",
            "[recipe]\nname = \"x\"\n[encode]\nlevel = 23\n",
            ErrorKind::OutOfRange,
            4,
        ),
        (
            "a dictionary id wider than u32",
            "[recipe]\nname = \"x\"\n[dictionary]\nid = 0x1_0000_0000\n",
            ErrorKind::OutOfRange,
            4,
        ),
        (
            "a window ceiling below the smallest expressible window",
            "[recipe]\nname = \"x\"\n[decode]\nmax_window = \"512\"\n",
            ErrorKind::OutOfRange,
            4,
        ),
        (
            "an unknown size suffix",
            "[recipe]\nname = \"x\"\n[decode]\nmax_decoded = \"8 furlongs\"\n",
            ErrorKind::BadSize,
            4,
        ),
        (
            "an unterminated string",
            "[recipe]\nname = \"x\n",
            ErrorKind::BadString,
            2,
        ),
        (
            "an unsupported escape",
            "[recipe]\nname = \"a\\u0041b\"\n",
            ErrorKind::BadString,
            2,
        ),
        (
            "a literal string",
            "[recipe]\nname = 'x'\n",
            ErrorKind::Unsupported,
            2,
        ),
        (
            "a multi-line string",
            "[recipe]\nname = \"\"\"x\"\"\"\n",
            ErrorKind::Unsupported,
            2,
        ),
        (
            "an inline table",
            "[recipe]\nname = \"x\"\n[dictionary]\nid = { a = 1 }\n",
            ErrorKind::Unsupported,
            4,
        ),
        (
            "a float",
            "[recipe]\nname = \"x\"\n[encode]\nlevel = 3.5\n",
            ErrorKind::Unsupported,
            4,
        ),
        (
            "a dotted key",
            "[recipe]\nname = \"x\"\n[encode]\na.b = 1\n",
            ErrorKind::Unsupported,
            4,
        ),
        (
            "a nested table header",
            "[recipe]\nname = \"x\"\n[encode.inner]\n",
            ErrorKind::Unsupported,
            3,
        ),
        (
            "a nested array",
            "[recipe]\nname = \"x\"\n[match]\ncontent_type = [[\"a\"]]\n",
            ErrorKind::Unsupported,
            4,
        ),
        (
            "an unclosed table header",
            "[recipe\nname = \"x\"\n",
            ErrorKind::BadTableHeader,
            1,
        ),
        (
            "a rule table written with single brackets",
            "[recipe]\nname = \"x\"\n[rule]\nstrategy = \"stored\"\n",
            ErrorKind::BadTableHeader,
            3,
        ),
        (
            "a line that is not an assignment",
            "[recipe]\nname = \"x\"\nnonsense\n",
            ErrorKind::Malformed,
            3,
        ),
        (
            "an array left open at end of file",
            "[recipe]\nname = \"x\"\n[match]\ncontent_type = [\n  \"a\",\n",
            ErrorKind::Malformed,
            5,
        ),
        (
            "two values with nothing between them",
            "[recipe]\nname = \"x\"\n[match]\ncontent_type = [\"a\" \"b\"]\n",
            ErrorKind::Malformed,
            4,
        ),
        (
            "trailing junk after a value",
            "[recipe]\nname = \"x\" oops\n",
            ErrorKind::Malformed,
            2,
        ),
        (
            "a manifest with no name",
            "[encode]\nlevel = 3\n",
            ErrorKind::Missing,
            0,
        ),
        (
            "an empty name",
            "[recipe]\nname = \"  \"\n",
            ErrorKind::Missing,
            2,
        ),
        (
            "a rule that matches nothing",
            "[recipe]\nname = \"x\"\n[[rule]]\nstrategy = \"stored\"\n",
            ErrorKind::EmptyRule,
            0,
        ),
    ];

    for (label, text, kind, line) in cases {
        let err = match parse(text) {
            Ok(recipe) => panic!("{label}: accepted, and should not have been: {recipe:?}"),
            Err(e) => e,
        };
        assert_eq!(err.kind, *kind, "{label}: wrong reason ({err})");
        assert_eq!(err.line, *line, "{label}: wrong line ({err})");
    }
}

#[test]
fn two_rules_may_each_set_the_same_key() {
    // The duplicate ledger is per-table, and each `[[rule]]` is its own table.
    // Getting this wrong would make the second rule in every manifest a
    // duplicate-key error.
    let text = r#"
[recipe]
name = "x"

[[rule]]
match_content_type = ["image/webp"]
strategy = "stored"

[[rule]]
match_content_type = ["image/png"]
strategy = "stored"
"#;
    let recipe = parse(text).expect("must parse");
    assert_eq!(recipe.rules.len(), 2);
    for ct in ["image/webp", "image/png"] {
        assert_eq!(
            recipe.encode_for(Some(ct), None).strategy,
            Strategy::Stored,
            "{ct}"
        );
    }
}

// ------------------------------------------------------------ loading

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// A real dictionary from the reference's own corpus, if it is present.
fn golden_dictionary() -> Option<Vec<u8>> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../zstd-fixtures/golden-dictionaries/http-dict-missing-symbols");
    fs::read(path).ok()
}

#[test]
fn a_dictionary_path_resolves_relative_to_its_manifest() {
    let Some(bytes) = golden_dictionary() else {
        eprintln!("zstd-fixtures/ not present; skipping");
        return;
    };
    let id = zalt_codec::dict::Dictionary::parse(&bytes)
        .expect("fixture parses")
        .id();

    let dir = scratch("dict-relative");
    fs::write(dir.join("sample.dict"), &bytes).unwrap();
    fs::write(
        dir.join("r.toml"),
        format!("[recipe]\nname = \"x\"\n[dictionary]\npath = \"sample.dict\"\nid = {id}\n"),
    )
    .unwrap();

    // Loaded by a path that is not the process's working directory, which is
    // the case the relative-to-manifest rule exists for.
    let loaded = zalt_recipe::load::load_recipe(dir.join("r.toml")).expect("must load");
    let dictionary = loaded.dictionary.expect("dictionary resolved");
    assert_eq!(dictionary.id(), id);
    assert!(!dictionary.content().is_empty());
}

#[test]
fn a_dictionary_whose_id_contradicts_the_manifest_is_refused() {
    let Some(bytes) = golden_dictionary() else {
        eprintln!("zstd-fixtures/ not present; skipping");
        return;
    };
    let real = zalt_codec::dict::Dictionary::parse(&bytes)
        .expect("fixture parses")
        .id();

    let dir = scratch("dict-mismatch");
    fs::write(dir.join("sample.dict"), &bytes).unwrap();
    fs::write(
        dir.join("r.toml"),
        format!(
            "[recipe]\nname = \"x\"\n[dictionary]\npath = \"sample.dict\"\nid = {}\n",
            real.wrapping_add(1)
        ),
    )
    .unwrap();

    // This is the failure that produces wrong bytes rather than an error, so
    // catching it at load time is the entire reason the manifest carries an id.
    let err = zalt_recipe::load::load_recipe(dir.join("r.toml")).unwrap_err();
    assert_eq!(err.kind, ErrorKind::DictionaryIdMismatch);
}

#[test]
fn a_manifest_naming_a_missing_dictionary_fails_to_load() {
    let dir = scratch("dict-missing");
    fs::write(
        dir.join("r.toml"),
        "[recipe]\nname = \"x\"\n[dictionary]\npath = \"absent.dict\"\n",
    )
    .unwrap();
    let err = zalt_recipe::load::load_recipe(dir.join("r.toml")).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Io);
}

#[test]
fn a_dictionary_named_only_by_id_loads_without_one() {
    // The well-known-id case from §7: nothing on disk to read, and that is not
    // an error — the recipe is waiting for a dictionary both peers resolve out
    // of band, not carrying a broken reference to one.
    let dir = scratch("dict-by-id");
    fs::write(
        dir.join("r.toml"),
        "[recipe]\nname = \"x\"\n[dictionary]\nid = 0x5A544D4C\n",
    )
    .unwrap();
    let loaded = zalt_recipe::load::load_recipe(dir.join("r.toml")).expect("must load");
    assert!(loaded.dictionary.is_none());
    assert_eq!(
        loaded.recipe.dictionary.as_ref().unwrap().source,
        DictionarySource::ById
    );
    assert_eq!(loaded.recipe.dictionary.unwrap().id, Some(0x5A54_4D4C));
}

#[test]
fn a_directory_loads_every_manifest_and_only_manifests() {
    let dir = scratch("directory");
    fs::write(
        dir.join("a.toml"),
        "[recipe]\nname = \"a\"\n[match]\ncontent_type = [\"application/a\"]\n",
    )
    .unwrap();
    fs::write(
        dir.join("b.toml"),
        "[recipe]\nname = \"b\"\n[match]\nextension = [\".b\"]\n",
    )
    .unwrap();
    // A dictionary sitting alongside the manifests. A loader that tried to
    // parse this as TOML would fail on a file doing exactly what it should.
    fs::write(dir.join("a.dict"), b"not a manifest").unwrap();
    fs::write(dir.join("README"), b"nor is this").unwrap();

    let set = zalt_recipe::load::load_directory(&dir).expect("must load");
    assert_eq!(set.len(), 2);
    assert_eq!(
        set.route(Some("application/a"), None).unwrap().recipe.name,
        "a"
    );
    assert_eq!(set.route(None, Some("x.b")).unwrap().recipe.name, "b");
}

#[test]
fn routing_yields_the_recipe_and_its_dictionary_together() {
    let Some(bytes) = golden_dictionary() else {
        eprintln!("zstd-fixtures/ not present; skipping");
        return;
    };
    let id = zalt_codec::dict::Dictionary::parse(&bytes)
        .expect("fixture parses")
        .id();

    let dir = scratch("route-with-dict");
    fs::write(dir.join("sample.dict"), &bytes).unwrap();
    fs::write(
        dir.join("a.toml"),
        format!(
            "[recipe]\nname = \"a\"\n[match]\ncontent_type = [\"application/a\"]\n\
             [dictionary]\npath = \"sample.dict\"\nid = {id}\n"
        ),
    )
    .unwrap();
    // A second recipe with no dictionary, so a set holding both has to keep
    // them straight rather than handing out the only one it has.
    fs::write(
        dir.join("b.toml"),
        "[recipe]\nname = \"b\"\n[match]\ncontent_type = [\"application/b\"]\n",
    )
    .unwrap();

    let set = zalt_recipe::load::load_directory(&dir).expect("must load");

    // One lookup, both halves. Pairing a recipe with the wrong dictionary is
    // the mistake that produces wrong bytes instead of an error, so the API
    // does not offer a way to join them up by hand and get it wrong.
    let a = set.route(Some("application/a"), None).expect("routes");
    assert_eq!(a.recipe.name, "a");
    assert_eq!(a.dictionary.as_ref().expect("resolved").id(), id);

    let b = set.route(Some("application/b"), None).expect("routes");
    assert_eq!(b.recipe.name, "b");
    assert!(b.dictionary.is_none());
}

#[test]
fn a_directory_with_two_claims_on_one_route_is_refused() {
    let dir = scratch("ambiguous");
    for name in ["a", "b"] {
        fs::write(
            dir.join(format!("{name}.toml")),
            format!("[recipe]\nname = \"{name}\"\n[match]\ncontent_type = [\"application/x\"]\n"),
        )
        .unwrap();
    }
    let err = zalt_recipe::load::load_directory(&dir).unwrap_err();
    assert_eq!(err.kind, ErrorKind::AmbiguousRoute);
}

#[test]
fn a_recipe_set_built_by_hand_routes_the_same_way_as_one_loaded() {
    // The parser is one producer of recipes, not their definition — a host that
    // bakes a recipe in as a Rust value must land in the same place.
    let parsed = parse(SKETCH).expect("sketch parses");
    let mut set = RecipeSet::new();
    set.insert(parsed).unwrap();
    assert_eq!(
        set.route(Some("application/ztml"), None)
            .unwrap()
            .recipe
            .name,
        "ztml-0.3"
    );
    assert_eq!(
        set.route(None, Some("paper.zdoc")).unwrap().recipe.name,
        "ztml-0.3"
    );
}
