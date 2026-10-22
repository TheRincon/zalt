//! Open Question Q1 (and Q7) of the ZLTA spec, tested against zstd's reference
//! seekable reader, the reference streaming decoder, and, when available, the
//! `zstd` CLI and a system `tar`.
//!
//! Run with `cargo test -- --nocapture` to see the per-test summaries.

mod common;

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use common::*;
use zlta_seekable_probe::{
    archive::{self, *},
    corpus,
    frame::{self, FrameKind},
    seekable::RefSeekable,
    stock,
    xxh64::xxh64,
};

// ------------------------------------------------------------------ Q1: spec profile

/// The §8.2 profile as written: the Archive Header is entry 0, skippable frames
/// are listed with Decompressed_Size 0, and the Central Index is the last entry.
#[test]
fn q1_spec_profile_is_read_correctly() {
    let c = corpus::standard(1);
    let a = build(&c, &small_frames());
    eprintln!("spec profile: {}", describe(&a));
    assert!(a.frames.iter().filter(|f| f.role == Role::Data).count() > 10);
    verify_reference_reader(&a, &c);
}

#[test]
fn q1_spec_profile_without_seek_checksums() {
    let c = corpus::standard(2);
    let a = build(
        &c,
        &WriterOptions {
            seek_checksums: false,
            ..small_frames()
        },
    );
    eprintln!("spec profile, no checksums: {}", describe(&a));
    verify_reference_reader(&a, &c);
}

/// §8.2 says the Checksum of a skippable entry MUST be 0, but XXH64 of empty
/// input is not 0. This checks whether the reference reader ever compares it.
#[test]
fn q1_checksum_of_zero_size_entries_is_never_consulted() {
    let c = corpus::standard(3);
    for sc in [
        SkippableChecksum::Zero,
        SkippableChecksum::EmptyInputHash,
        SkippableChecksum::Custom(0xDEAD_BEEF),
    ] {
        let o = WriterOptions {
            skippable_checksum: sc,
            vendor_every: Some(2),
            ..small_frames()
        };
        let a = build(&c, &o);
        eprintln!("skippable checksum {sc:?}: {}", describe(&a));
        verify_reference_reader(&a, &c);
    }
}

/// Header plus several dictionary-slot frames before the first data frame: a run
/// of zero-size entries at stream offset 0.
#[test]
fn q1_several_leading_skippable_frames() {
    let c = corpus::standard(4);
    let o = WriterOptions {
        leading_frames: vec![
            (MAGIC_DICT, vec![0xAB; 3000]),
            (MAGIC_DICT, vec![0xCD; 10]),
            (MAGIC_VENDOR, b"TESTleading".to_vec()),
        ],
        ..small_frames()
    };
    let a = build(&c, &o);
    eprintln!("4 leading skippables: {}", describe(&a));
    verify_reference_reader(&a, &c);
    assert!(stock::decompress_stream(&a.bytes).unwrap() == a.tar_stream);
}

/// Positive control for the test above: the reference reader *does* verify the
/// Checksum column of data frames, so "never consulted" for zero-size entries is
/// a real result, not checksums being ignored altogether.
#[test]
fn control_data_frame_checksums_are_verified() {
    let c = corpus::standard(12);
    let mut a = build(&c, &small_frames());
    let first_data = a
        .listed
        .iter()
        .position(|&i| a.frames[i].role == Role::Data)
        .unwrap();
    let pos = a.seek_table_offset() as usize + 8 + first_data * 12 + 8;
    a.bytes[pos] ^= 0x01;
    let mut zs = RefSeekable::open(&a.bytes).unwrap();
    let err = zs
        .read_at(0, a.tar_stream.len())
        .expect_err("corrupted data-frame checksum went unnoticed");
    eprintln!("control: flipped one bit of entry {first_data}'s checksum -> {err}");
}

// ------------------------------------------------------------------ Q7

/// Skippable frames *between* data frames, listed as zero-size entries.
#[test]
fn q7_skippable_frames_between_data_frames() {
    let c = corpus::standard(5);
    for every in [1, 3] {
        let a = build(
            &c,
            &WriterOptions {
                vendor_every: Some(every),
                ..small_frames()
            },
        );
        eprintln!(
            "vendor frame after every {every} data frame(s): {}",
            describe(&a)
        );
        verify_reference_reader(&a, &c);
        assert!(stock::decompress_stream(&a.bytes).unwrap() == a.tar_stream);
    }
}

// ------------------------------------------------------------------ stress

/// The reference reader loads the table through a 128 KiB buffer. Make a table
/// several times that size, with a zero-size entry after every data frame.
#[test]
fn q1_large_seek_table_with_interleaved_zero_size_entries() {
    let c = corpus::many_tiny(6000, 7);
    let o = WriterOptions {
        solid_target: 512,
        chunk: 512,
        vendor_every: Some(1),
        ..Default::default()
    };
    let a = build(&c, &o);
    let table_bytes = a.listed.len() * 12 + 9;
    eprintln!(
        "large table: {} ({} KiB of seek table)",
        describe(&a),
        table_bytes >> 10
    );
    assert!(table_bytes > 2 * (128 << 10));
    verify_reference_reader(&a, &c);
}

#[test]
fn empty_archive_is_terminator_only() {
    let c = corpus::empty();
    let a = build(&c, &small_frames());
    let roles: Vec<Role> = a.frames.iter().map(|f| f.role).collect();
    assert_eq!(roles, [Role::Header, Role::Terminator, Role::Index]);
    verify_reference_reader(&a, &c);
    assert!(stock::decompress_stream(&a.bytes).unwrap() == vec![0u8; TERMINATOR_LEN]);
}

// ------------------------------------------------------------------ the Q1 fallback

const CHILD_ENV: &str = "ZLTA_PROBE_CHILD";

/// The fallback named in Q1: list only data frames. An unmodified reference
/// reader then computes every file offset from 0, so it is off by the size of the
/// leading skippable frames.
///
/// The reference reader has live `assert()`s, so each case runs in a child
/// process: an abort is reported as a result instead of killing the test binary.
#[test]
fn q1_fallback_data_only_profile_is_misread() {
    if let Ok(case) = env::var(CHILD_ENV) {
        data_only_child(&case);
        return;
    }
    for case in ["checksums", "no-checksums"] {
        let out = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "q1_fallback_data_only_profile_is_misread",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, case)
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        // libtest may print "test <name> ... " on the same line as the first output.
        let lines: Vec<&str> = text
            .lines()
            .filter_map(|l| {
                l.find("VERDICT:")
                    .or_else(|| l.find("NOTE:"))
                    .map(|i| &l[i..])
            })
            .collect();
        eprintln!(
            "data-only profile, {case}: child exit {:?}",
            out.status.code()
        );
        for l in &lines {
            eprintln!("    {l}");
        }
        if !lines.iter().any(|l| l.starts_with("VERDICT")) {
            eprintln!(
                "    VERDICT: child aborted; stderr tail:\n{}",
                tail(&text, 6)
            );
        }
        assert!(
            !lines.iter().any(|l| l.contains("read OK")),
            "the data-only fallback unexpectedly works with the unmodified reference reader"
        );
    }
}

fn data_only_child(case: &str) {
    let c = corpus::standard(6);
    let o = WriterOptions {
        profile: SeekProfile::DataOnly,
        seek_checksums: case == "checksums",
        ..small_frames()
    };
    let a = build(&c, &o);
    let mut zs = match RefSeekable::open(&a.bytes) {
        Ok(zs) => zs,
        Err(e) => {
            println!("VERDICT: rejected at open: {e}");
            return;
        }
    };
    println!(
        "NOTE: reader places data frame 0 at file offset {}; it is really at {}",
        zs.frame_c_offset(0),
        a.frames[a.listed[0]].file_offset
    );
    match zs.read_at(0, a.tar_stream.len()) {
        Err(e) => println!("VERDICT: full read failed: {e}"),
        Ok(d) if d == a.tar_stream => println!("VERDICT: full read OK"),
        Ok(d) => println!(
            "VERDICT: full read returned WRONG DATA with no error ({} bytes, first difference at {})",
            d.len(),
            first_mismatch(&d, &a.tar_stream)
        ),
    }
    let mid = a.listed.len() as u32 / 2;
    let f = &a.frames[a.listed[mid as usize]];
    let want = &a.tar_stream[f.stream_offset as usize..(f.stream_offset + f.dsize) as usize];
    match zs.decompress_frame(mid) {
        Err(e) => println!("VERDICT: random access to frame {mid} failed: {e}"),
        Ok(d) if d == want => println!("VERDICT: random access to frame {mid} read OK"),
        Ok(_) => {
            println!("VERDICT: random access to frame {mid} returned WRONG DATA with no error")
        }
    }
}

fn tail(s: &str, n: usize) -> String {
    let v: Vec<&str> = s.lines().collect();
    v[v.len().saturating_sub(n)..].join("\n")
}

// ------------------------------------------------------------------ stock tools

#[test]
fn stock_decoder_sees_a_plain_tar_zst() {
    let c = corpus::standard(8);
    for o in [
        small_frames(),
        WriterOptions {
            vendor_every: Some(2),
            leading_frames: vec![(MAGIC_DICT, vec![1; 100])],
            ..small_frames()
        },
    ] {
        let a = build(&c, &o);
        let got = stock::decompress_stream(&a.bytes).unwrap();
        assert!(
            got == a.tar_stream,
            "stock streaming decode differs from the tar stream"
        );
    }
}

/// `zstd -t`, `zstd -dc`, `tar -tf`, `tar -xf` and `tar -I zstd`. Uses `ZSTD_BIN`
/// and `TAR_BIN` if set, otherwise `PATH`. Skips (with a message) if absent.
#[test]
fn stock_cli_zstd_and_tar() {
    let c = corpus::standard(9);
    let a = build(
        &c,
        &WriterOptions {
            vendor_every: Some(3),
            ..small_frames()
        },
    );
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("zlta-cli");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let archive_path = dir.join("probe.tar.zst");
    fs::write(&archive_path, &a.bytes).unwrap();

    let Some(zstd) = find_tool("ZSTD_BIN", "zstd") else {
        eprintln!("SKIPPED: no zstd CLI found (set ZSTD_BIN)");
        return;
    };
    let version = Command::new(&zstd).arg("--version").output().unwrap();
    eprintln!("using {}", String::from_utf8_lossy(&version.stdout).trim());

    let t = Command::new(&zstd)
        .args(["-q", "-t"])
        .arg(&archive_path)
        .status()
        .unwrap();
    assert!(t.success(), "zstd -t rejected the archive");
    let d = Command::new(&zstd)
        .args(["-q", "-d", "-c"])
        .arg(&archive_path)
        .output()
        .unwrap();
    assert!(
        d.status.success(),
        "zstd -dc failed: {}",
        String::from_utf8_lossy(&d.stderr)
    );
    assert!(
        d.stdout == a.tar_stream,
        "zstd -dc output differs from the tar stream"
    );

    // GNU tar first where it is installed under its Homebrew name: macOS's own
    // `tar` is bsdtar, which lists NFD paths in NFC (see below).
    let Some(tar) = find_tool("TAR_BIN", "gtar").or_else(|| find_tool("TAR_BIN", "tar")) else {
        eprintln!("SKIPPED tar checks: no tar found (set TAR_BIN)");
        return;
    };
    let bsdtar = Command::new(&tar)
        .arg("--version")
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("bsdtar"));
    let tar_path = dir.join("probe.tar");
    fs::write(&tar_path, &d.stdout).unwrap();
    let want: Vec<&str> = c.entries.iter().map(|e| e.path.as_str()).collect();

    let listing = Command::new(&tar)
        .arg("-tf")
        .arg(&tar_path)
        .output()
        .unwrap();
    assert!(
        listing.status.success(),
        "tar -tf: {}",
        String::from_utf8_lossy(&listing.stderr)
    );
    let listed = String::from_utf8(listing.stdout).unwrap();
    let got: Vec<String> = listed
        .lines()
        .map(|l| unescape_octal(l.trim_end_matches('/')))
        .collect();
    if bsdtar {
        // bsdtar on macOS presents decomposed (NFD) paths precomposed (NFC):
        // `donne\u{301}es` lists as `données`. The archive is right — §9.3
        // stores paths exactly as given — but §14.1's "with bsdtar" cannot be
        // checked byte for byte on such a path without a normalization table,
        // which this crate does not carry. So the non-ASCII paths are compared
        // by count, the rest exactly, and the gap is printed rather than hidden.
        eprintln!("note: bsdtar normalizes non-ASCII paths; comparing ASCII paths exactly");
        let ascii = |v: &[&str]| {
            v.iter()
                .filter(|p| p.is_ascii())
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
        };
        let got_refs: Vec<&str> = got.iter().map(String::as_str).collect();
        assert_eq!(
            ascii(&got_refs),
            ascii(&want),
            "tar -tf listing (ASCII paths)"
        );
        assert_eq!(got.len(), want.len(), "tar -tf listing (count)");
    } else {
        assert_eq!(got, want, "tar -tf listing");
    }

    // What a user would type: tar reads the .tar.zst through zstd.
    let piped = Command::new(&tar)
        .arg("--use-compress-program")
        // GNU tar adds `-d` when it reads through a compress program; bsdtar
        // does not, and runs it as a *compressor* — which fails the same way on
        // any .tar.zst, ZLTA or not. So bsdtar is handed the decompress flag.
        .arg(if bsdtar {
            format!("{} -d", zstd.display())
        } else {
            zstd.display().to_string()
        })
        .arg("-tf")
        .arg(&archive_path)
        .output()
        .unwrap();
    assert!(
        piped.status.success(),
        "tar -I zstd -tf: {}",
        String::from_utf8_lossy(&piped.stderr)
    );
    assert_eq!(String::from_utf8(piped.stdout).unwrap(), listed);

    let x = dir.join("x");
    fs::create_dir_all(&x).unwrap();
    let ex = Command::new(&tar)
        .arg("-xf")
        .arg(&tar_path)
        .arg("-C")
        .arg(&x)
        .output()
        .unwrap();
    assert!(
        ex.status.success(),
        "tar -xf: {}",
        String::from_utf8_lossy(&ex.stderr)
    );
    for (p, data) in &c.files {
        assert!(
            fs::read(x.join(p)).unwrap() == *data,
            "extracted content of {p}"
        );
    }
    assert_eq!(
        fs::read_link(x.join("proj/latest")).unwrap(),
        Path::new("assets/big.bin")
    );
    eprintln!("zstd -t, zstd -dc, tar -tf, tar -I zstd -tf and tar -xf all OK");
}

/// GNU tar prints non-ASCII bytes as `\ooo` in non-UTF-8 locales; bsdtar prints
/// them raw. Accept both.
fn unescape_octal(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 4 <= b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            out.push(
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 4]).unwrap(), 8).unwrap(),
            );
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

fn find_tool(var: &str, name: &str) -> Option<PathBuf> {
    if let Some(p) = env::var_os(var) {
        return Some(PathBuf::from(p));
    }
    env::split_paths(&env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

// ------------------------------------------------------------------ §11.3 and §5.2

/// Walking the file frame by frame (§11.3) finds exactly the frames the writer
/// produced, and rebuilds a byte-identical seek table.
#[test]
fn recovery_scan_rebuilds_the_seek_table() {
    let c = corpus::standard(10);
    let a = build(
        &c,
        &WriterOptions {
            vendor_every: Some(4),
            seek_checksums: false,
            ..small_frames()
        },
    );
    let scanned = frame::scan(&a.bytes).unwrap();
    assert_eq!(
        scanned.len(),
        a.frames.len() + 1,
        "all frames plus the seek table"
    );

    let mut rebuilt = Vec::new();
    for (s, f) in scanned.iter().zip(&a.frames) {
        assert_eq!((s.offset, s.csize), (f.file_offset, f.csize));
        let dsize = match (&s.kind, f.role) {
            (
                FrameKind::Zstd {
                    content_size: Some(n),
                    ..
                },
                r,
            ) if r.is_zstd() => *n,
            (FrameKind::Skippable { magic, .. }, r) if !r.is_zstd() => {
                let want = match r {
                    Role::Header => MAGIC_HEADER,
                    Role::Dictionary => MAGIC_DICT,
                    Role::Index => MAGIC_INDEX,
                    _ => MAGIC_VENDOR,
                };
                assert_eq!(*magic, want, "magic of {r:?}");
                0
            }
            (k, r) => panic!("frame kind {k:?} does not match role {r:?}"),
        };
        rebuilt.extend_from_slice(&(s.csize as u32).to_le_bytes());
        rebuilt.extend_from_slice(&(dsize as u32).to_le_bytes());
    }
    let st = scanned.last().unwrap();
    assert!(matches!(
        st.kind,
        FrameKind::Skippable {
            magic: MAGIC_SEEK_TABLE,
            ..
        }
    ));
    assert_eq!(st.offset + st.csize, a.bytes.len() as u64);
    rebuilt.extend_from_slice(&(a.frames.len() as u32).to_le_bytes());
    rebuilt.push(0);
    rebuilt.extend_from_slice(&SEEKABLE_FOOTER_MAGIC.to_le_bytes());
    assert!(
        rebuilt == a.bytes[st.offset as usize + 8..],
        "rebuilt seek table differs"
    );
}

/// §5.2 frame requirements, at several levels. This is the conformance check for a
/// custom encoder plugged in via `encoder_at()`. It also cross-checks the probe's
/// XXH64 against the checksum libzstd embeds in each frame.
#[test]
fn frame_conformance_section_5_2() {
    let c = corpus::standard(11);
    for level in [-5, 1, 3, 9, 19] {
        let mut enc = encoder_at(level);
        let a = archive::write(&c.body, &c.entries, enc.as_mut(), &small_frames()).unwrap();
        for f in a.frames.iter().filter(|f| f.role.is_zstd()) {
            let bytes = &a.bytes[f.file_offset as usize..(f.file_offset + f.csize) as usize];
            let (kind, csize) = frame::inspect(bytes).unwrap();
            assert_eq!(
                csize,
                f.csize,
                "{}: encode_frame must return exactly one frame",
                enc.describe()
            );
            let FrameKind::Zstd {
                content_size,
                window_size,
                has_checksum,
                dict_id,
            } = kind
            else {
                panic!("not a Zstandard frame")
            };
            assert_eq!(
                content_size,
                Some(f.dsize),
                "§5.2(2) Frame_Content_Size present and correct"
            );
            assert_eq!(f.dsize % 512, 0, "§5.2(3) multiple of 512");
            assert!(
                window_size <= 1 << 27,
                "§5.2(5) window {window_size} > 128 MiB"
            );
            assert_eq!(dict_id, 0, "§5.2(6) no dictionary");
            assert!(has_checksum, "§5.2 SHOULD: Content_Checksum_Flag");
            let embedded = u32::from_le_bytes(bytes[bytes.len() - 4..].try_into().unwrap());
            assert_eq!(
                embedded, f.content_xxh32,
                "probe XXH64 agrees with the frame's own checksum"
            );
        }
        eprintln!("level {level:>3}: {} ({})", describe(&a), enc.describe());
        verify_reference_reader(&a, &c);
    }
    assert_eq!(xxh64(&[], 0) as u32, 0x51D8_E999);
}
