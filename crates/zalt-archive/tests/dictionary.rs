//! Dictionary archives (§6): written, read back, judged by stock tools given
//! the dictionary, appended to, recovered, and broken on purpose.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use zalt_archive::layout::{FLAG_DICT_REQUIRED, MAGIC_DICTIONARY, MAGIC_HEADER};
use zalt_archive::read::Archive;
use zalt_archive::{
    Dictionary, Entry, Error, FramePolicy, Kind, ReadLimits, Writer, WriterOptions,
};

fn tool(candidates: &[&str]) -> Option<String> {
    candidates
        .iter()
        .find(|t| {
            Command::new(t)
                .arg("--version")
                .output()
                .is_ok_and(|o| o.status.success())
        })
        .map(|t| t.to_string())
}

struct Scratch(PathBuf);

impl Scratch {
    /// A directory of its own: tests run in parallel in one process, and
    /// several train a dictionary at once.
    fn new(tag: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "zalt-archive-dict-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Small documents sharing keys and vocabulary: what dictionaries are for.
fn document(i: u32) -> Vec<u8> {
    let kinds = ["library", "binary", "example", "benchmark", "test"];
    let licenses = ["MIT", "Apache-2.0", "MIT OR Apache-2.0", "BSD-3-Clause"];
    format!(
        "[package]\nname = \"crate-{i}\"\nversion = \"0.{}.{}\"\nedition = \"2024\"\n\
         description = \"a {} for compression, number {i}\"\nlicense = \"{}\"\n\
         repository = \"https://example.org/crate-{i}\"\n\n[dependencies]\n\
         serde = {{ version = \"1\", features = [\"derive\"] }}\nzalt-codec = \"0.{}\"\n",
        i % 7,
        i % 13,
        kinds[i as usize % kinds.len()],
        licenses[i as usize % licenses.len()],
        i % 3,
    )
    .into_bytes()
}

fn members(range: std::ops::Range<u32>) -> Vec<(Entry, Vec<u8>)> {
    range
        .map(|i| {
            let content = document(i);
            let mut e = Entry::new(Kind::File, format!("crates/c{i:04}/Cargo.toml").as_bytes());
            e.size = content.len() as u64;
            e.mtime_sec = 1_700_000_000;
            (e, content)
        })
        .collect()
}

/// A formatted dictionary: one zstd trains from documents like these, or —
/// where zstd is missing — the golden dictionary from the reference's own
/// set, which still exercises every path if not the ratio.
fn dictionary() -> Option<Dictionary> {
    if let Some(zstd) = tool(&["zstd"]) {
        let dir = Scratch::new("train");
        let mut files = Vec::new();
        for (i, (_, content)) in members(5000..5600).iter().enumerate() {
            let p = dir.0.join(format!("s{i}"));
            std::fs::write(&p, content).unwrap();
            files.push(p);
        }
        let out = dir.0.join("d");
        let ok = Command::new(zstd)
            .args(["-q", "--train", "--maxdict=16384", "-o"])
            .arg(&out)
            .args(&files)
            .status()
            .unwrap()
            .success();
        assert!(ok, "zstd --train failed");
        return Some(Dictionary::parse(std::fs::read(out).unwrap()).unwrap());
    }
    let golden = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../zstd-fixtures/golden-dictionaries/http-dict-missing-symbols");
    let raw = std::fs::read(golden).ok()?;
    Some(Dictionary::parse(raw).unwrap())
}

/// Small solid groups, the setting a dictionary exists to make affordable:
/// fast single-file extraction without paying for it in ratio.
fn options(dictionary: Option<Dictionary>) -> WriterOptions {
    WriterOptions {
        level: 9,
        policy: FramePolicy {
            small_file_threshold: 2048,
            solid_target: 4096,
            large_chunk: 8192,
        },
        threads: 4,
        creator: Some(b"dictionary test".to_vec()),
        dictionary,
        ..WriterOptions::default()
    }
}

fn write(members: &[(Entry, Vec<u8>)], options: WriterOptions) -> Vec<u8> {
    let mut w = Writer::new(Vec::new(), options).unwrap();
    for (e, content) in members {
        w.add(e.clone(), &mut content.as_slice()).unwrap();
    }
    w.finish().unwrap().0
}

fn open(bytes: Vec<u8>) -> zalt_archive::Result<Archive<[u8]>> {
    Archive::open(bytes.into_boxed_slice(), ReadLimits::default())
}

fn every_member_reads_back(archive: &Archive<[u8]>, members: &[(Entry, Vec<u8>)]) {
    archive.verify().unwrap();
    assert_eq!(archive.entries().len(), members.len());
    for ((want, content), got) in members.iter().zip(archive.entries()) {
        assert_eq!(got.path, want.path);
        assert_eq!(&archive.entry_bytes(got).unwrap(), content);
    }
}

/// The dictionary frame is second, the flag is set, every data frame names
/// the dictionary, and every member comes back.
#[test]
fn a_dictionary_archive_round_trips() {
    let Some(dict) = dictionary() else {
        eprintln!("SKIPPED: no zstd to train with and no golden dictionary");
        return;
    };
    let members = members(0..300);
    let bytes = write(&members, options(Some(dict.clone())));
    let archive = open(bytes.clone()).unwrap();
    assert_ne!(archive.header().flags & FLAG_DICT_REQUIRED, 0);
    assert_eq!(archive.dictionaries(), std::slice::from_ref(&dict));
    let frames = archive.frames();
    assert_eq!(frames[0].magic, MAGIC_HEADER);
    assert_eq!(frames[1].magic, MAGIC_DICTIONARY);
    for f in frames.iter().filter(|f| f.is_data()) {
        let frame = &bytes[f.file_offset as usize..(f.file_offset + f.compressed_size) as usize];
        let header =
            zalt_archive::frames::header_of(frame, &zalt_codec::params::DecodeLimits::default())
                .unwrap();
        assert_eq!(
            header.dictionary_id,
            Some(dict.id()),
            "every data frame names it"
        );
    }
    every_member_reads_back(&archive, &members);
}

/// What the dictionary is for: with small groups, the archive — dictionary
/// frame included — is smaller than the same archive without one.
#[test]
fn small_groups_cost_less_with_a_dictionary() {
    let Some(dict) = dictionary() else {
        eprintln!("SKIPPED: no dictionary");
        return;
    };
    if tool(&["zstd"]).is_none() {
        eprintln!("SKIPPED: only a trained dictionary is expected to pay");
        return;
    }
    let members = members(0..2000);
    let plain = write(&members, options(None)).len();
    let with = write(&members, options(Some(dict))).len();
    eprintln!("{with} bytes with the dictionary, {plain} without");
    assert!(
        with < plain,
        "{with} bytes with the dictionary, {plain} without"
    );
}

/// The thread count still cannot change a byte (§12.4).
#[test]
fn a_dictionary_archive_is_reproducible_at_any_parallelism() {
    let Some(dict) = dictionary() else {
        return;
    };
    let members = members(0..200);
    let one = write(
        &members,
        WriterOptions {
            threads: 1,
            ..options(Some(dict.clone()))
        },
    );
    let many = write(
        &members,
        WriterOptions {
            threads: 8,
            ..options(Some(dict))
        },
    );
    assert!(one == many, "the thread count changed the archive");
}

/// `zstd -dc` alone refuses a dictionary archive; given the dictionary — the
/// Dictionary frame's bytes — it decodes the tar stream, and tar lists it.
#[test]
fn stock_zstd_reads_a_dictionary_archive_given_the_dictionary() {
    let (Some(zstd), Some(tar)) = (tool(&["zstd"]), tool(&["gtar", "tar"])) else {
        eprintln!("SKIPPED: zstd or tar not on PATH");
        return;
    };
    let dict = dictionary().unwrap();
    let dir = Scratch::new("stock");
    let members = members(0..120);
    let archive = dir.0.join("a.tar.zst");
    std::fs::write(&archive, write(&members, options(Some(dict.clone())))).unwrap();
    let dict_path = dir.0.join("a.dict");
    std::fs::write(&dict_path, dict.raw()).unwrap();

    let alone = Command::new(&zstd)
        .args(["-q", "-dc"])
        .arg(&archive)
        .output()
        .unwrap();
    assert!(
        !alone.status.success(),
        "zstd decoded dictionary frames with no dictionary"
    );

    let stream = Command::new(&zstd)
        .args(["-q", "-dc", "-D"])
        .arg(&dict_path)
        .arg(&archive)
        .output()
        .unwrap();
    assert!(
        stream.status.success(),
        "{}",
        String::from_utf8_lossy(&stream.stderr)
    );
    let mut list = Command::new(&tar)
        .args(["-tf", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    list.stdin
        .take()
        .unwrap()
        .write_all(&stream.stdout)
        .unwrap();
    let listing = list.wait_with_output().unwrap();
    assert!(listing.status.success());
    let listed: Vec<String> = String::from_utf8_lossy(&listing.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    let ours: Vec<String> = members
        .iter()
        .map(|(e, _)| e.path_lossy().into_owned())
        .collect();
    assert_eq!(listed, ours);
}

/// Raw content cannot be an archive's dictionary: a frame names its
/// dictionary by ID, and raw content has none.
#[test]
fn raw_content_is_not_a_dictionary_here() {
    let raw = document(1).repeat(20);
    assert!(matches!(Dictionary::parse(raw), Err(Error::Format(_))));
}

/// With `DICT_REQUIRED` cleared, the reader loads no dictionary (§11.1 step
/// 7), and a frame that names one is refused when read — never decoded
/// without it.
#[test]
fn a_frame_naming_a_dictionary_the_reader_does_not_hold_is_refused() {
    let Some(dict) = dictionary() else {
        return;
    };
    let members = members(0..40);
    let mut bytes = write(&members, options(Some(dict)));
    // Archive_Flags: the header frame's User_Data at offset 8, flags at 6.
    bytes[8 + 6] &= !(FLAG_DICT_REQUIRED as u8);
    let archive = open(bytes).unwrap();
    assert!(archive.dictionaries().is_empty());
    let e = &archive.entries()[0];
    let err = archive.entry_bytes(e).unwrap_err();
    assert!(err.to_string().contains("does not hold"), "{err}");
    assert!(archive.verify().is_err());
}

#[test]
fn the_reader_bounds_what_dictionaries_it_will_hold() {
    let Some(dict) = dictionary() else {
        return;
    };
    let bytes = write(&members(0..10), options(Some(dict.clone())));
    let limits = ReadLimits {
        max_dictionary_bytes: dict.raw().len() as u64 / 2,
        ..ReadLimits::default()
    };
    let refused = Archive::open(bytes.into_boxed_slice(), limits);
    assert!(matches!(refused, Err(Error::Refused(_))), "{refused:?}");
}

/// Appending keeps using the archive's dictionary unasked, refuses one the
/// archive does not hold, and cannot give one to a portable archive (§10).
#[test]
fn appending_keeps_the_dictionary_and_cannot_add_one() {
    let Some(dict) = dictionary() else {
        return;
    };
    let dir = Scratch::new("append");
    let first = members(0..100);
    let second = members(100..160);
    let path = dir.0.join("a.tar.zst");
    std::fs::write(&path, write(&first, options(Some(dict.clone())))).unwrap();

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut w = zalt_archive::append(file, options(None)).unwrap();
    for (e, content) in &second {
        w.add(e.clone(), &mut content.as_slice()).unwrap();
    }
    w.finish().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let archive = open(bytes.clone()).unwrap();
    let all: Vec<_> = first.iter().chain(&second).cloned().collect();
    every_member_reads_back(&archive, &all);
    for f in archive.frames().iter().filter(|f| f.is_data()) {
        let frame = &bytes[f.file_offset as usize..(f.file_offset + f.compressed_size) as usize];
        let header =
            zalt_archive::frames::header_of(frame, &zalt_codec::params::DecodeLimits::default())
                .unwrap();
        assert_eq!(
            header.dictionary_id,
            Some(dict.id()),
            "appended frames use it too"
        );
    }

    // A dictionary the archive does not hold: refused, archive untouched.
    let other = {
        let mut raw = dict.raw().to_vec();
        raw[4..8].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        Dictionary::parse(raw).unwrap()
    };
    let before = std::fs::read(&path).unwrap();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let refused = zalt_archive::append(file, options(Some(other)));
    assert!(matches!(refused, Err(Error::Refused(_))));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "a refused append changes nothing"
    );

    // A portable archive cannot gain one.
    let portable = dir.0.join("p.tar.zst");
    std::fs::write(&portable, write(&first, options(None))).unwrap();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&portable)
        .unwrap();
    assert!(matches!(
        zalt_archive::append(file, options(Some(dict))),
        Err(Error::Refused(_))
    ));
}

/// Recovery of a dictionary archive that lost its trailer gets every member
/// back, with the dictionary; one whose dictionary frame was damaged loses
/// the members written against it, and says so.
#[test]
fn recovery_keeps_the_dictionary_and_loses_what_needed_a_damaged_one() {
    let Some(dict) = dictionary() else {
        return;
    };
    let members = members(0..150);
    let bytes = write(&members, options(Some(dict.clone())));
    let archive = open(bytes.clone()).unwrap();
    let terminator = *archive.frames().iter().rev().find(|f| f.is_data()).unwrap();
    let dict_frame = archive.frames()[1];
    drop(archive);

    // The trailer gone: cut at the Terminator.
    let cut = bytes[..terminator.file_offset as usize].to_vec();
    let (out, recovered) =
        zalt_archive::recover(cut.as_slice(), Vec::new(), options(None)).unwrap();
    assert!(recovered.lost.is_empty(), "{:?}", recovered.lost.len());
    let rebuilt = open(out).unwrap();
    assert_eq!(rebuilt.dictionaries(), std::slice::from_ref(&dict));
    every_member_reads_back(&rebuilt, &members);

    // The dictionary damaged — its magic number, so it no longer loads.
    let mut damaged = bytes.clone();
    damaged[dict_frame.file_offset as usize + 8] ^= 0xff;
    let (out, recovered) =
        zalt_archive::recover(damaged.as_slice(), Vec::new(), options(None)).unwrap();
    assert_eq!(
        recovered.lost.len(),
        members.len(),
        "every member needed it"
    );
    let rebuilt = open(out).unwrap();
    assert!(rebuilt.dictionaries().is_empty());
    assert!(rebuilt.entries().is_empty());
}

/// The probe's independent §7 decoder — libzstd underneath, and no
/// dictionary given — reads a dictionary archive's Central Index exactly as
/// ours does: §6 keeps the index free of dictionaries, and this is the
/// reference saying so.
#[test]
fn the_reference_reads_the_index_without_the_dictionary() {
    let Some(dict) = dictionary() else {
        return;
    };
    let bytes = write(&members(0..80), options(Some(dict)));
    let ours = open(bytes.clone()).unwrap();
    let last = *ours.frames().last().unwrap();
    let frame =
        &bytes[last.file_offset as usize..(last.file_offset + last.compressed_size) as usize];
    let theirs = zlta_seekable_probe::index::decode(&frame[8..], 64 << 20)
        .unwrap_or_else(|e| panic!("the probe cannot read the Central Index: {e}"));
    assert_eq!(theirs.tar_stream_size, ours.tar_stream_size());
    assert_eq!(theirs.entries.len(), ours.entries().len());
    for (t, o) in theirs.entries.iter().zip(ours.entries()) {
        assert_eq!(t.path.as_bytes(), o.path.as_slice());
        assert_eq!(t.header_offset, o.header_offset);
        assert_eq!(t.data_offset(), o.data_offset);
        assert_eq!(t.size, o.size);
        assert_eq!(t.hash, o.hash);
    }
}
