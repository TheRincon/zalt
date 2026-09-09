//! What the training tool must get right, checked without the reference.
//!
//! The measurement subcommands need `zstd` and are exercised by `just
//! ztml-ratio` and `just ztml-sweep`. What is checked here is everything that
//! decides *what bytes get written*, because those are the parts whose being
//! wrong is silent: a dictionary assembled in a different order still works, a
//! manifest that emits a subtly different recipe still parses.

use std::path::{Path, PathBuf};
use std::process::Command;

fn binary() -> PathBuf {
    // The integration test binary lives beside the tool's binary.
    let mut p = std::env::current_exe().expect("test binary path");
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("zalt-recipe-train")
}

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// Three samples of deliberately different sizes, so ordering is observable.
fn corpus(dir: &Path) -> Vec<PathBuf> {
    let specs = [
        ("big.txt", 300usize, b'C'),
        ("small.txt", 100, b'A'),
        ("mid.txt", 200, b'B'),
    ];
    let mut out = Vec::new();
    for (name, len, byte) in specs {
        let path = dir.join(name);
        std::fs::write(&path, vec![byte; len]).unwrap();
        out.push(path);
    }
    out
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(binary())
        .args(args)
        .output()
        .expect("the tool runs")
}

#[test]
fn samples_are_ordered_by_size_regardless_of_the_order_given() {
    // The measured reason this matters: the same bytes in a different order
    // changed compressed output by up to 3.7%, because zstd matches backwards
    // from the dictionary's end. A build that depended on argument order — or
    // on shell glob order — would be a different artifact per invocation.
    let dir = scratch("ordering");
    let files = corpus(&dir);
    let paths: Vec<String> = files.iter().map(|p| p.display().to_string()).collect();

    let forward: Vec<&str> = paths.iter().map(String::as_str).collect();
    let mut backward = forward.clone();
    backward.reverse();

    let a = dir.join("a");
    let b = dir.join("b");
    let mut args = vec!["build", "--out", a.to_str().unwrap(), "--name", "d"];
    args.extend_from_slice(&forward);
    assert!(run(&args).status.success());

    let mut args = vec!["build", "--out", b.to_str().unwrap(), "--name", "d"];
    args.extend_from_slice(&backward);
    assert!(run(&args).status.success());

    let one = std::fs::read(a.join("d.dict")).unwrap();
    let two = std::fs::read(b.join("d.dict")).unwrap();
    assert_eq!(one, two, "argument order changed the artifact");

    // Ascending size, so the largest sample is nearest the end where the
    // offsets are cheapest.
    assert_eq!(
        one,
        b"A".repeat(100)
            .into_iter()
            .chain(b"B".repeat(200))
            .chain(b"C".repeat(300))
            .collect::<Vec<u8>>()
    );
}

#[test]
fn a_size_budget_trims_the_front_and_keeps_the_tail() {
    // The direction is the whole point. The tail is where a content-prefix
    // dictionary earns its ratio, so a budget must discard the front — trimming
    // the tail would throw away exactly what a size-constrained dictionary most
    // needs to keep.
    let dir = scratch("budget");
    let files = corpus(&dir);
    let paths: Vec<String> = files.iter().map(|p| p.display().to_string()).collect();

    let out = dir.join("o");
    let mut args = vec![
        "build",
        "--out",
        out.to_str().unwrap(),
        "--name",
        "d",
        "--max-size",
        "400",
    ];
    args.extend(paths.iter().map(String::as_str));
    assert!(run(&args).status.success());

    let blob = std::fs::read(out.join("d.dict")).unwrap();
    assert_eq!(blob.len(), 400);
    // 600 bytes total, trimmed to 400: the 100 A's and half the B's are gone.
    assert_eq!(
        blob,
        b"B".repeat(100)
            .into_iter()
            .chain(b"C".repeat(300))
            .collect::<Vec<u8>>()
    );
    assert!(blob.ends_with(&b"C".repeat(300)), "the tail must survive");

    let prov = std::fs::read_to_string(out.join("d.provenance.txt")).unwrap();
    assert!(
        prov.contains("trimmed 200 bytes"),
        "a trim must be recorded: {prov}"
    );
}

#[test]
fn the_provenance_records_the_bytes_that_were_written() {
    let dir = scratch("provenance");
    let files = corpus(&dir);
    let paths: Vec<String> = files.iter().map(|p| p.display().to_string()).collect();
    let out = dir.join("o");
    let mut args = vec!["build", "--out", out.to_str().unwrap(), "--name", "d"];
    args.extend(paths.iter().map(String::as_str));
    assert!(run(&args).status.success());

    let blob = std::fs::read(out.join("d.dict")).unwrap();
    let prov = std::fs::read_to_string(out.join("d.provenance.txt")).unwrap();

    let recorded_len: usize = prov
        .lines()
        .find_map(|l| l.strip_prefix("bytes"))
        .expect("a length")
        .trim()
        .parse()
        .unwrap();
    assert_eq!(recorded_len, blob.len());

    let recorded_hash = prov
        .lines()
        .find_map(|l| l.strip_prefix("sha256"))
        .expect("a hash")
        .trim()
        .to_string();
    let actual = String::from_utf8(zalt_recipe::sha256::Sha256::digest(&blob).to_hex().to_vec())
        .expect("ascii");
    assert_eq!(
        recorded_hash, actual,
        "the provenance hash is not the blob's"
    );
}

#[test]
fn an_emitted_manifest_parses_back_to_what_it_described() {
    // The round trip that justifies having an emitter at all. A manifest whose
    // text the parser rejects is a file nothing can read; one that parses to a
    // *different* recipe is worse, because it loads and is wrong.
    let dir = scratch("manifest");
    let files = corpus(&dir);
    let paths: Vec<String> = files.iter().map(|p| p.display().to_string()).collect();
    let out = dir.join("o");
    let manifest = dir.join("out.toml");

    let mut args = vec![
        "build",
        "--out",
        out.to_str().unwrap(),
        "--name",
        "sample-0.1",
        "--level",
        "9",
        "--manifest",
        manifest.to_str().unwrap(),
    ];
    args.extend(paths.iter().map(String::as_str));
    let result = run(&args);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );

    let text = std::fs::read_to_string(&manifest).unwrap();
    let parsed = zalt_recipe::parse(&text).expect("the emitted manifest must parse");
    assert_eq!(parsed.name, "sample-0.1");
    assert_eq!(parsed.encode.level, Some(9));
    assert_eq!(
        parsed.dictionary.expect("a dictionary is named").source,
        zalt_recipe::DictionarySource::Path("sample-0.1.dict".into())
    );
}

#[test]
fn an_existing_manifest_is_never_overwritten() {
    // The one irreversible thing this tool could do. A hand-written manifest
    // carries the prose explaining why each number is what it is, and no
    // generator can reproduce that.
    let dir = scratch("no-clobber");
    let files = corpus(&dir);
    let paths: Vec<String> = files.iter().map(|p| p.display().to_string()).collect();
    let out = dir.join("o");
    let manifest = dir.join("existing.toml");
    std::fs::write(&manifest, "# hand written, do not lose\n").unwrap();

    let mut args = vec![
        "build",
        "--out",
        out.to_str().unwrap(),
        "--name",
        "d",
        "--manifest",
        manifest.to_str().unwrap(),
    ];
    args.extend(paths.iter().map(String::as_str));
    let result = run(&args);

    assert!(!result.status.success(), "overwriting must fail");
    assert_eq!(
        std::fs::read_to_string(&manifest).unwrap(),
        "# hand written, do not lose\n"
    );
}

#[test]
fn leave_one_out_refuses_a_corpus_too_small_to_hold_anything_out() {
    // With one sample the leave-one-out dictionary is empty, so every "gain"
    // would be zero and the run would look like a dictionary that does nothing
    // rather than a measurement that could not be taken.
    let dir = scratch("too-small");
    let one = dir.join("only.txt");
    std::fs::write(&one, b"just the one").unwrap();
    let result = run(&["eval", one.to_str().unwrap()]);
    assert!(!result.status.success());
    let err = String::from_utf8_lossy(&result.stderr);
    assert!(
        err.contains("at least two samples") || err.contains("zstd"),
        "unexpected error: {err}"
    );
}

#[test]
fn a_missing_sample_is_an_error_not_an_empty_dictionary() {
    let dir = scratch("missing");
    let result = run(&[
        "build",
        "--out",
        dir.to_str().unwrap(),
        "--name",
        "d",
        "nope.txt",
    ]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("not a file"));
}
