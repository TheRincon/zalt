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
    // A child that rejects its arguments exits *without reading stdin*, and the
    // write then lands on a pipe with no reader. That is the behaviour under
    // test for every usage-error case here, not a failure of the harness.
    //
    // It passed locally by luck: these payloads are far smaller than a pipe
    // buffer, so the write usually completes before the child gets round to
    // exiting. CI lost that race and the test panicked with `BrokenPipe`, which
    // is a flaky test rather than a real one — the outcome depended on
    // scheduling.
    //
    // Only `BrokenPipe` is tolerated. Any other write error is still a genuine
    // harness failure and still panics.
    if let Err(e) = child
        .stdin
        .as_mut()
        .expect("stdin was piped")
        .write_all(input)
    {
        assert_eq!(
            e.kind(),
            std::io::ErrorKind::BrokenPipe,
            "writing to the child's stdin failed for a reason other than an \
             early exit: {e}"
        );
    }
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

// ---------------------------------------------------------------- compress

/// Text with structure at several distances, so every level has real choices.
fn compressible() -> Vec<u8> {
    let mut v = Vec::new();
    for i in 0..1500u32 {
        v.extend_from_slice(
            format!(
                "<row id=\"{i}\" kind=\"{}\">value {}</row>\n",
                i % 7,
                (i * 13) % 101
            )
            .as_bytes(),
        );
    }
    v
}

/// The checksum flag, read from the frame header the way a decoder reads it:
/// bit 2 of the descriptor byte that follows the four-byte magic.
fn declares_checksum(frame: &[u8]) -> bool {
    assert_eq!(&frame[..4], &[0x28, 0xB5, 0x2F, 0xFD], "not a zstd frame");
    frame[4] & 0b100 != 0
}

#[test]
fn every_level_round_trips_through_the_cli() {
    let data = compressible();
    let mut sizes = Vec::new();
    for level in 1..=19 {
        let flag = format!("-{level}");
        let packed = zalt_stdin(&["c", "-q", &flag], &data);
        assert_eq!(code(&packed), 0, "level {level}: {}", stderr(&packed));
        assert!(packed.stderr.is_empty(), "-q must keep stderr quiet");
        let unpacked = zalt_stdin(&["d", "-q"], &packed.stdout);
        assert_eq!(code(&unpacked), 0, "level {level}: {}", stderr(&unpacked));
        assert_eq!(unpacked.stdout, data, "level {level} did not round trip");
        sizes.push(packed.stdout.len());
    }
    // Not a strict ordering — the levels trade differently — but the top
    // must beat the bottom, or the level never reached the encoder.
    assert!(
        sizes[18] < sizes[0],
        "level 19 ({}) should be smaller than level 1 ({})",
        sizes[18],
        sizes[0]
    );
}

#[test]
fn the_long_level_flag_and_the_short_one_agree() {
    let data = compressible();
    let short = zalt_stdin(&["c", "-q", "-17"], &data);
    let long = zalt_stdin(&["compress", "-q", "--level", "17"], &data);
    assert_eq!(code(&short), 0);
    assert_eq!(
        short.stdout, long.stdout,
        "the same level must give the same bytes"
    );
}

#[test]
fn a_checksum_is_written_by_default_and_can_be_declined() {
    let data = compressible();
    let default = zalt_stdin(&["c", "-q"], &data);
    let declined = zalt_stdin(&["c", "-q", "--no-check"], &data);
    let explicit = zalt_stdin(&["c", "-q", "--check"], &data);
    assert!(
        declares_checksum(&default.stdout),
        "zstd(1)'s default, and ours"
    );
    assert!(!declares_checksum(&declined.stdout));
    assert!(declares_checksum(&explicit.stdout));
    assert_eq!(
        default.stdout.len(),
        declined.stdout.len() + 4,
        "four bytes of XXH64"
    );
}

#[test]
fn levels_outside_what_is_implemented_are_refused_not_clamped() {
    for (args, wanted) in [
        (&["c", "-0"][..], "1..=19"),
        (&["c", "-20"][..], "not implemented"),
        (&["c", "-22"][..], "8 MiB"),
        (&["c", "--level", "99"][..], "1..=19"),
        (&["c", "--level", "abc"][..], "not a compression level"),
        (&["c", "-123"][..], "unknown option"),
    ] {
        let out = zalt_stdin(args, b"x");
        assert_eq!(code(&out), EXIT_USAGE, "{args:?}");
        assert!(stderr(&out).contains(wanted), "{args:?}: {}", stderr(&out));
    }
}

#[test]
fn a_flag_given_to_the_wrong_direction_is_refused() {
    for args in [
        &["c", "--budget", "1M"][..],
        &["c", "--trusted"][..],
        &["c", "--recycle"][..],
        &["c", "--max-decoded", "1M"][..],
    ] {
        let out = zalt_stdin(args, b"x");
        assert_eq!(code(&out), EXIT_USAGE, "{args:?}");
        assert!(
            stderr(&out).contains("decoding"),
            "{args:?}: {}",
            stderr(&out)
        );
    }
    for args in [
        &["d", "-19"][..],
        &["d", "--stored"][..],
        &["d", "--no-check"][..],
    ] {
        let out = zalt_stdin(args, HELLO_ZST);
        assert_eq!(code(&out), EXIT_USAGE, "{args:?}");
        assert!(
            stderr(&out).contains("compress only"),
            "{args:?}: {}",
            stderr(&out)
        );
    }
    let out = zalt_stdin(&["c", "--stored", "-9"], b"x");
    assert_eq!(
        code(&out),
        EXIT_USAGE,
        "a level with --stored is a contradiction"
    );
}

#[test]
fn stored_output_is_the_content_plus_headers_and_still_decodes() {
    let data = compressible();
    let out = zalt_stdin(&["c", "-q", "--stored", "--no-check"], &data);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(out.stdout.len() > data.len(), "stored does not compress");
    assert!(out.stdout.len() < data.len() + 32, "only headers on top");
    let back = zalt_stdin(&["d", "-q"], &out.stdout);
    assert_eq!(back.stdout, data);
}

#[test]
fn several_inputs_become_frames_that_decode_to_their_concatenation() {
    let dir = std::env::temp_dir().join(format!("zalt-cli-c-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let a = dir.join("a.txt");
    let b = dir.join("b.txt");
    fs::write(&a, b"first file\n".repeat(50)).unwrap();
    fs::write(&b, compressible()).unwrap();
    let (a, b) = (a.to_str().unwrap(), b.to_str().unwrap());

    let packed = zalt(&["c", "-q", "-9", a, b]);
    assert_eq!(code(&packed), 0, "{}", stderr(&packed));
    let unpacked = zalt_stdin(&["d", "-q"], &packed.stdout);
    let mut expected = b"first file\n".repeat(50);
    expected.extend_from_slice(&compressible());
    assert_eq!(unpacked.stdout, expected);

    let refused = zalt(&["c", "-q", a, b, "-o", dir.join("x.zst").to_str().unwrap()]);
    assert_eq!(code(&refused), EXIT_USAGE, "one --output, two inputs");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn compress_will_not_overwrite_without_force() {
    let dir = std::env::temp_dir().join(format!("zalt-cli-o-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let target = dir.join("out.zst");
    fs::write(&target, b"precious").unwrap();
    let t = target.to_str().unwrap();

    let refused = zalt_stdin(&["c", "-q", "-o", t], b"data");
    assert_eq!(code(&refused), EXIT_INPUT, "{}", stderr(&refused));
    assert_eq!(
        fs::read(&target).unwrap(),
        b"precious",
        "must not have been touched"
    );

    let forced = zalt_stdin(&["c", "-q", "-f", "-o", t], b"data");
    assert_eq!(code(&forced), 0, "{}", stderr(&forced));
    let back = zalt(&["d", "-q", t]);
    assert_eq!(back.stdout, b"data");
    let _ = fs::remove_dir_all(&dir);
}

/// The test that matters most for an encoder: the reference reads it.
/// Skipped, loudly, where `zstd` is not installed.
#[test]
fn the_reference_decodes_what_the_cli_compresses() {
    let Ok(version) = Command::new("zstd").arg("--version").output() else {
        eprintln!("SKIP: zstd(1) not on PATH");
        return;
    };
    assert!(version.status.success());
    let data = compressible();
    for level in ["-1", "-5", "-9", "-16", "-19"] {
        let packed = zalt_stdin(&["c", "-q", level], &data);
        assert_eq!(code(&packed), 0);
        let mut child = Command::new("zstd")
            .args(["-q", "-d", "-c"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(&packed.stdout)
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "zstd refused level {level}");
        assert_eq!(out.stdout, data, "zstd decoded level {level} wrongly");
    }
}

#[test]
fn the_summary_goes_to_stderr_and_names_the_level() {
    let out = zalt_stdin(&["c", "-12"], &compressible());
    assert_eq!(code(&out), 0);
    let summary = stderr(&out);
    assert!(summary.contains("level 12"), "{summary}");
    assert!(summary.contains("checksum"), "{summary}");
    assert!(
        declares_checksum(&out.stdout),
        "stdout is the frame and nothing else"
    );
}

// ----------------------------------------------------------------- archive

struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("zalt-cli-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn s(&self, rel: &str) -> String {
        self.0.join(rel).to_str().unwrap().to_string()
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn tree(at: &Path) {
    fs::create_dir_all(at.join("proj/src")).unwrap();
    fs::write(at.join("proj/src/a.txt"), compressible()).unwrap();
    fs::write(
        at.join("proj/b.bin"),
        (0..70_000u32)
            .map(|i| (i * 7 % 251) as u8)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    std::os::unix::fs::symlink("src/a.txt", at.join("proj/latest")).unwrap();
}

#[test]
fn an_archive_round_trips_through_the_cli() {
    let t = TempDir::new("arch");
    tree(&t.0);
    let archive = t.s("a.tar.zst");
    let made = Command::new(ZALT)
        .args(["archive", "create", "-q", "-9", "-o", &archive, "proj"])
        .current_dir(&t.0)
        .output()
        .unwrap();
    assert_eq!(code(&made), 0, "{}", stderr(&made));

    let listed = zalt(&["archive", "list", &archive]);
    assert_eq!(code(&listed), 0, "{}", stderr(&listed));
    assert_eq!(
        stdout(&listed).lines().collect::<Vec<_>>(),
        [
            "proj",
            "proj/b.bin",
            "proj/latest",
            "proj/src",
            "proj/src/a.txt"
        ]
    );
    assert_eq!(code(&zalt(&["a", "v", "-q", &archive])), 0);

    let out = t.s("out");
    let x = zalt(&["archive", "extract", "-q", &archive, "-C", &out]);
    assert_eq!(code(&x), 0, "{}", stderr(&x));
    assert_eq!(
        fs::read(t.0.join("out/proj/src/a.txt")).unwrap(),
        compressible()
    );
    assert_eq!(
        fs::read_link(t.0.join("out/proj/latest")).unwrap(),
        Path::new("src/a.txt")
    );

    // A plain zstd stream too: the CLI's own decompress reads it whole, which
    // it could not before a stream ending in skippable frames was accepted.
    let stream = zalt(&["d", "-q", &archive]);
    assert_eq!(code(&stream), 0, "{}", stderr(&stream));
    assert_eq!(stream.stdout.len() % 512, 0, "a tar stream");
}

/// macOS metadata through the CLI, inspected by the system's own `xattr(1)`.
#[cfg(target_os = "macos")]
#[test]
fn archive_metadata_and_quarantine_through_the_cli() {
    let t = TempDir::new("archmeta");
    tree(&t.0);
    let xattr = |args: &[&str]| {
        let out = Command::new("/usr/bin/xattr").args(args).output().unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        )
    };
    let a = t.s("proj/src/a.txt");
    assert!(xattr(&["-w", "com.zalt.note", "kept", &a]).0);

    let make = |name: &str, extra: &[&str]| {
        let archive = t.s(name);
        let made = Command::new(ZALT)
            .args(["a", "c", "-q", "-o", &archive])
            .args(extra)
            .arg("proj")
            .current_dir(&t.0)
            .output()
            .unwrap();
        assert_eq!(code(&made), 0, "{}", stderr(&made));
        archive
    };
    let with = make("with.tar.zst", &[]);
    let without = make("without.tar.zst", &["--no-xattrs"]);
    // The archive is quarantined, as a download would be.
    assert!(xattr(&["-w", "com.apple.quarantine", "0081;zalt;test", &with]).0);

    let out = t.s("out");
    let x = zalt(&["a", "x", "-q", &with, "-C", &out]);
    assert_eq!(code(&x), 0, "{}", stderr(&x));
    let got = t.s("out/proj/src/a.txt");
    assert_eq!(xattr(&["-p", "com.zalt.note", &got]), (true, "kept".into()));
    assert_eq!(
        xattr(&["-p", "com.apple.quarantine", &got]),
        (true, "0081;zalt;test".into())
    );

    let bare = t.s("bare");
    let x = zalt(&["a", "x", "-q", &without, "-C", &bare]);
    assert_eq!(code(&x), 0, "{}", stderr(&x));
    assert!(!xattr(&["-p", "com.zalt.note", &t.s("bare/proj/src/a.txt")]).0);
}

#[test]
fn appending_through_the_cli_and_a_refused_append_changes_nothing() {
    let t = TempDir::new("archapp");
    tree(&t.0);
    fs::create_dir_all(t.0.join("more")).unwrap();
    fs::write(t.0.join("more/new.txt"), b"appended\n").unwrap();
    let archive = t.s("a.tar.zst");
    let run = |args: &[&str]| {
        Command::new(ZALT)
            .args(args)
            .current_dir(&t.0)
            .output()
            .unwrap()
    };
    let made = run(&["a", "c", "-q", "-o", &archive, "proj"]);
    assert_eq!(code(&made), 0, "{}", stderr(&made));

    let grown = run(&["a", "r", "-q", &archive, "more"]);
    assert_eq!(code(&grown), 0, "{}", stderr(&grown));
    assert_eq!(code(&zalt(&["a", "v", "-q", &archive])), 0);
    let listed = stdout(&zalt(&["a", "t", &archive]));
    assert!(listed.lines().any(|l| l == "proj/src/a.txt"), "{listed}");
    assert!(listed.lines().any(|l| l == "more/new.txt"), "{listed}");

    // Appending what is already there is refused, and the archive is
    // byte-for-byte what it was, with no copy left beside it.
    let before = fs::read(&archive).unwrap();
    let again = run(&["a", "r", "-q", &archive, "more"]);
    assert_eq!(code(&again), EXIT_INPUT, "{}", stderr(&again));
    assert!(
        stderr(&again).contains("already in the archive"),
        "{}",
        stderr(&again)
    );
    assert_eq!(fs::read(&archive).unwrap(), before);
    let leftovers: Vec<_> = fs::read_dir(&t.0)
        .unwrap()
        .map(|d| d.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("zalt-append"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn recovering_through_the_cli() {
    let t = TempDir::new("archrec");
    tree(&t.0);
    let archive = t.s("a.tar.zst");
    let made = Command::new(ZALT)
        .args(["a", "c", "-q", "-o", &archive, "proj"])
        .current_dir(&t.0)
        .output()
        .unwrap();
    assert_eq!(code(&made), 0);

    // Intact: said so, and nothing written.
    let fixed = t.s("fixed.tar.zst");
    let r = zalt(&["a", "recover", &archive, "-o", &fixed]);
    assert_eq!(code(&r), 0, "{}", stderr(&r));
    assert!(stderr(&r).contains("intact"), "{}", stderr(&r));
    assert!(!t.0.join("fixed.tar.zst").exists());

    // The trailer cut off, as a crash or a short copy leaves it.
    let bytes = fs::read(&archive).unwrap();
    let cut = t.s("cut.tar.zst");
    fs::write(&cut, &bytes[..bytes.len() - 200]).unwrap();
    assert_ne!(code(&zalt(&["a", "v", "-q", &cut])), 0);
    let r = zalt(&["a", "recover", &cut, "-o", &fixed]);
    assert_eq!(code(&r), 0, "{}", stderr(&r));
    assert!(stderr(&r).contains("members recovered"), "{}", stderr(&r));
    assert_eq!(code(&zalt(&["a", "v", "-q", &fixed])), 0);
    assert_eq!(
        stdout(&zalt(&["a", "t", &fixed])),
        stdout(&zalt(&["a", "t", &archive]))
    );
    assert_eq!(
        fs::read(&cut).unwrap(),
        &bytes[..bytes.len() - 200],
        "input untouched"
    );

    // An output that exists is not overwritten without -f.
    let r = zalt(&["a", "recover", &cut, "-o", &fixed]);
    assert_eq!(code(&r), EXIT_INPUT);
    // And something that is not an archive at all leaves no output behind.
    let junk = t.s("junk");
    fs::write(&junk, b"not an archive").unwrap();
    let nothing = t.s("nothing.tar.zst");
    assert_eq!(
        code(&zalt(&["a", "recover", &junk, "-o", &nothing])),
        EXIT_INPUT
    );
    assert!(!t.0.join("nothing.tar.zst").exists());
}

#[test]
fn extracting_named_members_takes_them_and_what_is_under_them() {
    let t = TempDir::new("archsel");
    tree(&t.0);
    let archive = t.s("a.tar.zst");
    let made = Command::new(ZALT)
        .args(["a", "c", "-q", "-o", &archive, "proj"])
        .current_dir(&t.0)
        .output()
        .unwrap();
    assert_eq!(code(&made), 0);
    let out = t.s("out");
    let x = zalt(&["a", "x", "-q", &archive, "-C", &out, "proj/src"]);
    assert_eq!(code(&x), 0, "{}", stderr(&x));
    assert!(t.0.join("out/proj/src/a.txt").exists());
    assert!(!t.0.join("out/proj/b.bin").exists());

    let missing = zalt(&["a", "x", "-q", &archive, "-C", &out, "proj/nope"]);
    assert_eq!(code(&missing), EXIT_INPUT);
    assert!(stderr(&missing).contains("not in the archive"));
}

#[test]
fn archive_commands_refuse_what_does_not_apply() {
    let t = TempDir::new("archref");
    for (args, code_wanted, wanted) in [
        (vec!["archive", "create", "proj"], EXIT_USAGE, "-o"),
        (
            vec!["archive", "create", "-o", "x"],
            EXIT_USAGE,
            "at least one path",
        ),
        (
            vec!["archive", "list", "-9", "x"],
            EXIT_USAGE,
            "does not apply",
        ),
        (
            vec!["archive", "extract", "x", "--threads", "2"],
            EXIT_USAGE,
            "does not apply",
        ),
        (
            vec!["archive", "create", "-0", "-o", "x", "p"],
            EXIT_USAGE,
            "1..=19",
        ),
        (
            vec!["archive", "frobnicate"],
            EXIT_USAGE,
            "unknown archive command",
        ),
        (
            vec!["archive", "list", "/nonexistent/x.tar.zst"],
            EXIT_INPUT,
            "nonexistent",
        ),
    ] {
        let out = Command::new(ZALT)
            .args(&args)
            .current_dir(&t.0)
            .output()
            .unwrap();
        assert_eq!(code(&out), code_wanted, "{args:?}: {}", stderr(&out));
        assert!(stderr(&out).contains(wanted), "{args:?}: {}", stderr(&out));
    }
}

#[test]
fn a_plain_zstd_file_is_not_mistaken_for_an_archive() {
    let t = TempDir::new("archnot");
    let plain = t.s("plain.zst");
    fs::write(&plain, HELLO_ZST).unwrap();
    let out = zalt(&["archive", "list", &plain]);
    assert_eq!(code(&out), EXIT_INPUT);
    assert!(
        stderr(&out).contains("not a valid ZSTA archive"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn create_will_not_overwrite_an_archive_without_force() {
    let t = TempDir::new("archforce");
    tree(&t.0);
    fs::write(t.0.join("a.tar.zst"), b"precious").unwrap();
    let out = Command::new(ZALT)
        .args(["a", "c", "-q", "-o", "a.tar.zst", "proj"])
        .current_dir(&t.0)
        .output()
        .unwrap();
    assert_eq!(code(&out), EXIT_INPUT);
    assert_eq!(fs::read(t.0.join("a.tar.zst")).unwrap(), b"precious");
}
