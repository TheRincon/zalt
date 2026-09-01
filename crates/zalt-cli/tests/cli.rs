//! End-to-end tests: run the real binary, the way a shell does.
//!
//! Deliberately not unit tests of the internals. The things worth pinning here
//! are the ones a caller depends on and a refactor can silently break — exit
//! codes, what lands on stdout versus stderr, whether output is byte-exact, and
//! whether a truncated file is noticed. None of those are observable from
//! inside the process.
//!
//! `CARGO_BIN_EXE_zalt` is set by cargo for integration tests of a crate with a
//! binary, so no path guessing and no test-only dependency.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const ZALT: &str = env!("CARGO_BIN_EXE_zalt");

/// Usage errors exit 2, input failures exit 1. A script driving this tool has
/// to be able to tell "I typed it wrong" from "that file is broken".
const EXIT_INPUT: i32 = 1;
const EXIT_USAGE: i32 = 2;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<name> is two levels below the root")
        .to_path_buf()
}

fn fixtures() -> Option<PathBuf> {
    let dir = workspace_root().join("crates/zalt-codec/tests/fixtures");
    dir.join("hello.zst").is_file().then_some(dir)
}

/// The reference corpus is optional: a checkout that has not fetched it should
/// skip rather than fail.
fn reference_corpus() -> Option<PathBuf> {
    let dir = workspace_root().join("zstd-fixtures");
    dir.is_dir().then_some(dir)
}

fn zalt(args: &[&str]) -> Output {
    Command::new(ZALT)
        .args(args)
        .output()
        .expect("the binary cargo just built must be runnable")
}

fn zalt_stdin(args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(ZALT)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin was piped")
        .write_all(input)
        .expect("write");
    child.wait_with_output().expect("wait")
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `zstd -1` of "hello world", with a content checksum.
const HELLO_ZST: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x24, 0x0b, 0x59, 0x00, 0x00, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x20, 0x77,
    0x6f, 0x72, 0x6c, 0x64, 0x68, 0x69, 0x1e, 0xb2,
];

// -------------------------------------------------------------- basics

#[test]
fn help_and_version_succeed_and_go_to_stdout() {
    for args in [&["help"][..], &["--help"][..], &[][..]] {
        let out = zalt(args);
        assert_eq!(code(&out), 0, "args {args:?}");
        assert!(stdout(&out).contains("USAGE"), "args {args:?}");
    }

    let out = zalt(&["--version"]);
    assert_eq!(code(&out), 0);
    let text = stdout(&out);
    assert!(text.contains("zalt"), "{text}");
    assert!(
        text.contains("backend:"),
        "the backend in use is worth saying"
    );
}

#[test]
fn an_unknown_option_is_a_usage_error() {
    let out = zalt(&["--no-such-flag"]);
    assert_eq!(code(&out), EXIT_USAGE);
    assert!(stderr(&out).contains("unknown option"));
}

#[test]
fn an_unknown_command_is_a_usage_error() {
    let out = zalt(&["frobnicate"]);
    assert_eq!(code(&out), EXIT_USAGE);
    assert!(stderr(&out).contains("unknown command"));
}

#[test]
fn a_flag_missing_its_value_is_a_usage_error() {
    let out = zalt(&["d", "--max-window-log"]);
    assert_eq!(code(&out), EXIT_USAGE);
    assert!(stderr(&out).contains("requires a value"));
}

#[test]
fn an_out_of_range_window_log_is_refused_before_anything_is_read() {
    for bad in ["9", "32", "41", "255"] {
        let out = zalt(&["d", "--max-window-log", bad]);
        assert_eq!(code(&out), EXIT_USAGE, "windowLog {bad}");
    }
}

// ------------------------------------------------------------ decompress

#[test]
fn decompresses_to_stdout() {
    let out = zalt_stdin(&["d", "-q"], HELLO_ZST);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(out.stdout, b"hello world");
}

#[test]
fn the_short_flag_spelling_works_too() {
    let out = zalt_stdin(&["-d", "-q"], HELLO_ZST);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(out.stdout, b"hello world");
}

/// The summary belongs on stderr, or piping decoded output into another program
/// would corrupt it.
#[test]
fn the_summary_never_contaminates_the_decoded_stream() {
    let out = zalt_stdin(&["d"], HELLO_ZST);
    assert_eq!(
        out.stdout, b"hello world",
        "stdout must be only the payload"
    );
    assert!(
        stderr(&out).contains("->"),
        "and the summary must still appear"
    );
}

#[test]
fn quiet_suppresses_the_summary() {
    let out = zalt_stdin(&["d", "-q"], HELLO_ZST);
    assert!(stderr(&out).is_empty(), "got {:?}", stderr(&out));
}

/// The property the streaming API exists for: suspend and resume at *arbitrary*
/// byte boundaries. A one-byte input buffer splits every header field, every
/// bitstream and every block boundary there is.
#[test]
fn output_is_identical_at_every_input_chunk_size() {
    let Some(dir) = fixtures() else { return };
    let compressed = dir.join("text.zst");
    let expected = fs::read(dir.join("text.bin")).expect("fixture pair");

    for chunk in ["1", "2", "3", "7", "13", "64", "4096", "1MiB"] {
        let out = zalt(&["d", "-q", "--in-buf", chunk, compressed.to_str().unwrap()]);
        assert_eq!(code(&out), 0, "chunk {chunk}: {}", stderr(&out));
        assert_eq!(
            out.stdout, expected,
            "chunk size {chunk} changed the output"
        );
    }
}

#[test]
fn concatenated_frames_decode_as_one_stream() {
    let mut both = HELLO_ZST.to_vec();
    both.extend_from_slice(HELLO_ZST);
    let out = zalt_stdin(&["d"], &both);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(out.stdout, b"hello worldhello world");
    assert!(stderr(&out).contains("2 frames"));
}

#[test]
fn an_empty_input_is_not_a_failure() {
    let out = zalt_stdin(&["d", "-q"], b"");
    assert_eq!(code(&out), 0);
    assert!(out.stdout.is_empty());
}

// ------------------------------------------------------------- refusals

/// A frame that stops mid-stream must be an error, not a short read reported as
/// success — a caller writing the result to a file would otherwise keep a
/// truncated document and never know.
#[test]
fn truncation_is_an_error_at_every_cut_point() {
    for cut in 1..HELLO_ZST.len() {
        let out = zalt_stdin(&["d", "-q"], &HELLO_ZST[..cut]);
        assert_eq!(
            code(&out),
            EXIT_INPUT,
            "a {cut}-byte prefix was accepted as a whole frame"
        );
    }
}

/// The amplification primitive from §6: a spec-legal six-byte header demanding
/// a 2 TiB window. It must be refused at header parse, not attempted.
#[test]
fn the_window_amplification_header_is_refused() {
    let frame = [0x28, 0xb5, 0x2f, 0xfd, 0x00, 0xff, 0x00, 0x00, 0x00];
    let out = zalt_stdin(&["d"], &frame);
    assert_eq!(code(&out), EXIT_INPUT);
    let msg = stderr(&out);
    assert!(msg.contains("windowLog"), "{msg}");
    assert!(
        msg.contains("41"),
        "the refusal should name what was asked: {msg}"
    );
}

#[test]
fn a_corrupted_checksum_is_refused() {
    let mut frame = HELLO_ZST.to_vec();
    let last = frame.len() - 1;
    frame[last] ^= 0xFF;
    let out = zalt_stdin(&["d", "-q"], &frame);
    assert_eq!(code(&out), EXIT_INPUT);
}

#[test]
fn a_file_that_is_not_a_frame_is_refused() {
    let out = zalt_stdin(&["d"], b"#!/bin/sh\necho not a zstd file\n");
    assert_eq!(code(&out), EXIT_INPUT);
    assert!(
        stderr(&out).contains("not a modern zstd frame"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn a_missing_file_is_an_input_error_not_a_panic() {
    let out = zalt(&["d", "/nonexistent/path/to/nothing.zst"]);
    assert_eq!(code(&out), EXIT_INPUT);
    assert!(!stderr(&out).contains("panic"), "{}", stderr(&out));
}

// --------------------------------------------------------------- output

#[test]
fn writes_to_an_output_file() {
    let dir = std::env::temp_dir().join(format!("zalt-cli-out-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("hello.txt");
    let src = dir.join("hello.zst");
    fs::write(&src, HELLO_ZST).unwrap();

    let out = zalt(&[
        "d",
        "-q",
        src.to_str().unwrap(),
        "-o",
        path.to_str().unwrap(),
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(fs::read(&path).unwrap(), b"hello world");

    // Overwriting is refused: `-o` pointed at the wrong path is a plausible way
    // to destroy an input, and the loss is not recoverable.
    let again = zalt(&[
        "d",
        "-q",
        src.to_str().unwrap(),
        "-o",
        path.to_str().unwrap(),
    ]);
    assert_eq!(code(&again), EXIT_INPUT);
    assert!(stderr(&again).contains("--force"));

    let forced = zalt(&[
        "d",
        "-q",
        "-f",
        src.to_str().unwrap(),
        "-o",
        path.to_str().unwrap(),
    ]);
    assert_eq!(code(&forced), 0, "{}", stderr(&forced));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn output_with_several_inputs_is_a_usage_error() {
    let out = zalt(&["d", "a.zst", "b.zst", "-o", "out"]);
    assert_eq!(code(&out), EXIT_USAGE);
}

// ----------------------------------------------------------------- info

#[test]
fn info_reports_a_frames_structure() {
    let dir = std::env::temp_dir().join(format!("zalt-cli-info-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("hello.zst");
    fs::write(&path, HELLO_ZST).unwrap();

    let out = zalt(&["info", path.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    for expected in [
        "zstd frame @0",
        "content size   11",
        "checksum       yes",
        "dictionary     none",
        "blocks         1",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn info_without_a_file_is_a_usage_error() {
    let out = zalt(&["info"]);
    assert_eq!(code(&out), EXIT_USAGE);
}

/// `info` reports what a file says; it must survive being pointed at rubbish.
#[test]
fn info_on_a_non_frame_reports_rather_than_fails() {
    let dir = std::env::temp_dir().join(format!("zalt-cli-junk-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("junk.bin");
    fs::write(&path, b"definitely not a zstd frame").unwrap();

    let out = zalt(&["info", path.to_str().unwrap()]);
    assert_eq!(code(&out), 0);
    assert!(stdout(&out).contains("trailing"), "{}", stdout(&out));

    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- check

#[test]
fn check_without_a_file_is_a_usage_error() {
    let out = zalt(&["check"]);
    assert_eq!(code(&out), EXIT_USAGE);
}

/// The direction that matters for security. Round-trip tests only ever prove a
/// decoder is permissive enough; this proves it is not *too* permissive.
#[test]
fn check_refuses_every_frame_in_the_golden_error_corpus() {
    let Some(root) = reference_corpus() else {
        return;
    };
    let dir = root.join("golden-decompression-errors");
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };

    let files: Vec<String> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "zst"))
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    if files.is_empty() {
        return;
    }

    let mut args = vec!["check", "-q"];
    args.extend(files.iter().map(String::as_str));
    let out = zalt(&args);

    assert_eq!(
        code(&out),
        EXIT_INPUT,
        "every frame here must be refused; got:\n{}",
        stdout(&out)
    );
    assert!(
        !stdout(&out).contains("ok    "),
        "a frame that must be refused was accepted:\n{}",
        stdout(&out)
    );
}

#[test]
fn check_accepts_every_frame_in_the_golden_decompression_corpus() {
    let Some(root) = reference_corpus() else {
        return;
    };
    let dir = root.join("golden-decompression");
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };

    let files: Vec<String> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "zst"))
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    if files.is_empty() {
        return;
    }

    // These are the reference's own frames and some declare large windows for
    // reasons that have nothing to do with a browser's policy.
    let mut args = vec!["check", "-q", "--trusted"];
    args.extend(files.iter().map(String::as_str));
    let out = zalt(&args);

    assert_eq!(code(&out), 0, "{}\n{}", stdout(&out), stderr(&out));
}

/// One bad input must not stop the run: the useful output over a corpus is the
/// whole list of what broke, not the first entry.
#[test]
fn check_reports_every_input_before_failing() {
    let dir = std::env::temp_dir().join(format!("zalt-cli-check-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let good = dir.join("good.zst");
    let bad = dir.join("bad.zst");
    fs::write(&good, HELLO_ZST).unwrap();
    fs::write(&bad, b"not a frame").unwrap();

    let out = zalt(&[
        "check",
        bad.to_str().unwrap(),
        good.to_str().unwrap(),
        bad.to_str().unwrap(),
    ]);

    assert_eq!(code(&out), EXIT_INPUT);
    let text = stdout(&out);
    assert_eq!(text.matches("FAIL").count(), 2, "{text}");
    assert_eq!(text.matches("ok    ").count(), 1, "{text}");
    assert!(stderr(&out).contains("2 of 3"), "{}", stderr(&out));

    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- policy

/// A budget too small to hold the decoder must fail the decode, not silently
/// allocate anyway.
#[test]
fn a_tiny_budget_refuses_the_decode() {
    let out = zalt_stdin(&["d", "-q", "--budget", "4"], HELLO_ZST);
    assert_eq!(code(&out), EXIT_INPUT);
    assert!(stderr(&out).contains("budget"), "{}", stderr(&out));
}

#[test]
fn an_ample_budget_changes_nothing_about_the_output() {
    let out = zalt_stdin(&["d", "-q", "--budget", "64MiB"], HELLO_ZST);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(out.stdout, b"hello world");
}

/// Recycling must be invisible in the output. It is a memory strategy, and if
/// it were ever observable in the bytes that would be the bug.
#[test]
fn recycling_buffers_does_not_change_the_output() {
    let Some(dir) = fixtures() else { return };
    let src = dir.join("text.zst");
    let expected = fs::read(dir.join("text.bin")).expect("fixture pair");

    let plain = zalt(&["d", "-q", src.to_str().unwrap()]);
    let recycled = zalt(&[
        "d",
        "-q",
        "--recycle",
        "--budget",
        "64MiB",
        src.to_str().unwrap(),
    ]);

    assert_eq!(code(&plain), 0);
    assert_eq!(code(&recycled), 0, "{}", stderr(&recycled));
    assert_eq!(plain.stdout, expected);
    assert_eq!(recycled.stdout, expected, "recycling changed the output");
}

/// Several inputs through one recycler is the case the free list exists for,
/// and the one where a stale-buffer bug would show up.
#[test]
fn a_shared_recycler_serves_many_inputs_correctly() {
    let Some(dir) = fixtures() else { return };
    let src = dir.join("hello.zst");
    let path = src.to_str().unwrap();

    let out = zalt(&[
        "d",
        "-q",
        "--recycle",
        "--budget",
        "64MiB",
        path,
        path,
        path,
        path,
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(out.stdout, b"hello worldhello worldhello worldhello world");
}

#[test]
fn size_suffixes_are_accepted() {
    for size in ["1024", "64K", "8MiB", "8mb", "1G"] {
        let out = zalt_stdin(&["d", "-q", "--budget", size], HELLO_ZST);
        assert_eq!(code(&out), 0, "budget {size}: {}", stderr(&out));
    }
    for bad in ["", "abc", "8QiB", "-5"] {
        let out = zalt_stdin(&["d", "-q", "--budget", bad], HELLO_ZST);
        assert_eq!(code(&out), EXIT_USAGE, "budget {bad:?} should be refused");
    }
}
