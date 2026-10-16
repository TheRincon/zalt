//! R2 (§14.2): appending (§10) and recovery (§11.3).

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use zalt_archive::read::Archive;
use zalt_archive::{
    Entry, Error, FramePolicy, Kind, ReadLimits, Writer, WriterOptions, append, recover,
};

/// Small frames, so a few kilobytes make many of them: solid groups, files of
/// their own, files chunked across frames.
fn options() -> WriterOptions {
    WriterOptions {
        level: 3,
        policy: FramePolicy {
            small_file_threshold: 4096,
            solid_target: 8192,
            large_chunk: 4096,
        },
        threads: 2,
        creator: Some(b"r2 test".to_vec()),
        hash: zalt_archive::Hash::default(),
    }
}

fn pseudo(n: usize, seed: u32) -> Vec<u8> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s >> 24) as u8
        })
        .collect()
}

fn file(path: &str, content: Vec<u8>) -> (Entry, Vec<u8>) {
    let mut e = Entry::new(Kind::File, path.as_bytes());
    e.size = content.len() as u64;
    e.mtime_sec = 1_700_000_000;
    (e, content)
}

fn first_batch() -> Vec<(Entry, Vec<u8>)> {
    let mut dir = Entry::new(Kind::Directory, "a");
    dir.mode = 0o755;
    let mut link = Entry::new(Kind::Symlink, "a/link");
    link.link = Some(b"one.txt".to_vec());
    vec![
        (dir, vec![]),
        file("a/one.txt", b"one\n".repeat(50)),
        file("a/two.bin", pseudo(1500, 2)),
        file("a/medium.bin", pseudo(6000, 3)),
        (link, vec![]),
        file("a/large.bin", pseudo(15_000, 4)),
        file("a/three.txt", b"three\n".to_vec()),
    ]
}

fn second_batch() -> Vec<(Entry, Vec<u8>)> {
    let mut hard = Entry::new(Kind::HardLink, "b/again");
    hard.link = Some(b"a/one.txt".to_vec());
    vec![
        file("b/four.txt", b"four\n".repeat(30)),
        file("b/big.bin", pseudo(13_000, 5)),
        (hard, vec![]),
        file("b/five.txt", pseudo(700, 6)),
    ]
}

fn write(members: &[(Entry, Vec<u8>)]) -> Vec<u8> {
    let mut w = Writer::new(Vec::new(), options()).unwrap();
    for (e, content) in members {
        w.add(e.clone(), &mut content.as_slice()).unwrap();
    }
    w.finish().unwrap().0
}

fn open(bytes: &[u8]) -> Archive<[u8]> {
    Archive::open(bytes.to_vec().into_boxed_slice(), ReadLimits::default()).unwrap()
}

/// Every member's content, by path.
fn contents(a: &Archive<[u8]>) -> Vec<(Vec<u8>, Vec<u8>)> {
    a.entries()
        .iter()
        .map(|e| (e.path.clone(), a.entry_bytes(e).unwrap()))
        .collect()
}

struct TempFile(PathBuf);
impl TempFile {
    fn with(bytes: &[u8]) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "zalt-r2-{}-{}.tar.zst",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&p, bytes).unwrap();
        Self(p)
    }
    fn rw(&self) -> std::fs::File {
        std::fs::File::options()
            .read(true)
            .write(true)
            .open(&self.0)
            .unwrap()
    }
    fn bytes(&self) -> Vec<u8> {
        std::fs::read(&self.0).unwrap()
    }
}
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn append_to(bytes: &[u8], members: &[(Entry, Vec<u8>)], options: WriterOptions) -> Vec<u8> {
    let t = TempFile::with(bytes);
    let mut w = append(t.rw(), options).unwrap();
    for (e, content) in members {
        w.add(e.clone(), &mut content.as_slice()).unwrap();
    }
    let (mut out, _) = w.finish().unwrap();
    out.flush().unwrap();
    drop(out);
    t.bytes()
}

/// File offset of the Terminator: the data frame before the Central Index.
fn terminator_offset(bytes: &[u8]) -> u64 {
    let a = open(bytes);
    let f = a.frames();
    f[f.len() - 2].file_offset
}

// ----------------------------------------------------------------- append

#[test]
fn appending_keeps_every_old_byte_and_record_and_adds_the_new() {
    let first = first_batch();
    let before = write(&first);
    let after = append_to(&before, &second_batch(), options());

    let a = open(&after);
    a.verify().unwrap();
    // §10 step 4: old records unchanged, since their offsets do not move.
    let old = open(&before);
    assert_eq!(&a.entries()[..old.entries().len()], old.entries());
    assert_eq!(a.entries().len(), first.len() + second_batch().len());
    // Everything before the old Terminator is the same bytes.
    let cut = terminator_offset(&before) as usize;
    assert_eq!(&after[..cut], &before[..cut]);
    // And every member reads back.
    let all: Vec<_> = first.iter().chain(second_batch().iter()).cloned().collect();
    for ((e, content), (path, got)) in all.iter().zip(contents(&a)) {
        assert_eq!(e.path, path);
        if e.kind == Kind::File {
            assert_eq!(&got, content, "{}", e.path_lossy());
        }
    }
}

#[test]
fn appending_to_an_empty_archive_and_appending_twice_at_other_levels() {
    let empty = write(&[]);
    let once = append_to(&empty, &first_batch(), options());
    let twice = append_to(
        &once,
        &second_batch(),
        WriterOptions {
            level: 19,
            ..options()
        },
    );
    let a = open(&twice);
    a.verify().unwrap();
    assert_eq!(
        a.entries().len(),
        first_batch().len() + second_batch().len()
    );
    // The Archive Header — and its Creator — is never rewritten.
    assert_eq!(&twice[..20], &empty[..20]);
}

#[test]
fn a_file_that_is_not_an_archive_is_left_untouched() {
    let t = TempFile::with(b"not an archive at all, but long enough to look at");
    assert!(append(t.rw(), options()).is_err());
    assert_eq!(
        t.bytes(),
        b"not an archive at all, but long enough to look at"
    );
}

#[test]
fn a_duplicate_path_is_refused_and_recovery_restores_the_archive() {
    let before = write(&first_batch());
    let t = TempFile::with(&before);
    let mut w = append(t.rw(), options()).unwrap();
    let (dup, content) = file("a/one.txt", b"again".to_vec());
    let refused = w.add(dup, &mut content.as_slice());
    assert!(matches!(refused, Err(Error::Refused(_))), "{refused:?}");
    drop(w);

    // §10: between the cut and `finish` the file has no trailer.
    let cut = t.bytes();
    assert!(Archive::open(cut.clone().into_boxed_slice(), ReadLimits::default()).is_err());
    let (fixed, report) = recover(cut.as_slice(), Vec::new(), options()).unwrap();
    let a = open(&fixed);
    a.verify().unwrap();
    assert_eq!(a.entries(), open(&before).entries());
    assert!(!report.terminator_found, "the old Terminator was cut away");
}

#[test]
fn a_new_archive_refuses_a_duplicate_path_too() {
    let mut w = Writer::new(Vec::new(), options()).unwrap();
    let (e, c) = file("x", b"1".to_vec());
    w.add(e.clone(), &mut c.as_slice()).unwrap();
    assert!(matches!(
        w.add(e, &mut c.as_slice()),
        Err(Error::Refused(_))
    ));
    // The refusal came before anything was written, so the writer finishes.
    let a = open(&w.finish().unwrap().0);
    assert_eq!(a.entries().len(), 1);
}

// --------------------------------------------------------------- recovery

/// Recover `bytes`, check the result is a whole archive whose members are the
/// first `k` of `original`'s with the same content, and return `k`.
fn recovered_prefix(bytes: &[u8], original: &Archive<[u8]>) -> zalt_archive::Result<usize> {
    let (fixed, report) = recover(bytes, Vec::new(), options())?;
    let a = open(&fixed);
    a.verify().unwrap();
    let k = a.entries().len();
    assert_eq!(report.entries as usize, k);
    assert_eq!(a.entries(), &original.entries()[..k], "not a prefix");
    for e in a.entries() {
        assert_eq!(a.entry_bytes(e).unwrap(), original.entry_bytes(e).unwrap());
    }
    assert_eq!(report.kept + report.dropped, bytes.len() as u64);
    Ok(k)
}

#[test]
fn every_truncation_recovers_a_prefix_that_only_grows() {
    let bytes = write(&first_batch());
    let original = open(&bytes);
    // Every frame boundary and the bytes around it, plus a stride through
    // the middles: each cut a crash or a short copy could leave.
    let mut cuts: Vec<usize> = Vec::new();
    for f in original.frames() {
        for d in [-9i64, -1, 0, 1, 7] {
            cuts.push((f.file_offset as i64 + d).clamp(0, bytes.len() as i64) as usize);
        }
    }
    cuts.extend((0..bytes.len()).step_by(61));
    cuts.push(bytes.len());
    cuts.sort_unstable();
    cuts.dedup();

    let header = original.frames()[0].compressed_size as usize;
    let mut last = 0;
    for cut in cuts {
        match recovered_prefix(&bytes[..cut], &original) {
            Ok(k) => {
                assert!(cut >= header);
                assert!(
                    k >= last,
                    "cut {cut}: {k} members, fewer than {last} at a shorter cut"
                );
                last = k;
            }
            Err(e) => assert!(cut < header, "cut {cut}: {e}"),
        }
    }
    assert_eq!(last, original.entries().len());
}

#[test]
fn a_lost_trailer_and_a_lost_terminator_both_recover_every_member() {
    let bytes = write(&first_batch());
    let original = open(&bytes);
    let frames = original.frames();
    let index = frames[frames.len() - 1].file_offset as usize;
    let terminator = frames[frames.len() - 2].file_offset as usize;

    let (_, report) = recover(&bytes[..index], Vec::new(), options()).unwrap();
    assert!(report.terminator_found && report.stopped.is_none());
    assert_eq!(
        recovered_prefix(&bytes[..index], &original).unwrap(),
        original.entries().len()
    );
    let (_, report) = recover(&bytes[..terminator], Vec::new(), options()).unwrap();
    assert!(!report.terminator_found);
    assert_eq!(
        recovered_prefix(&bytes[..terminator], &original).unwrap(),
        original.entries().len()
    );
}

#[test]
fn a_damaged_trailer_is_rebuilt() {
    let bytes = write(&first_batch());
    let original = open(&bytes);
    let index = original.frames()[original.frames().len() - 1].file_offset as usize;
    for at in [index + 20, bytes.len() - 1] {
        let mut bad = bytes.clone();
        bad[at] ^= 0x40;
        assert!(Archive::open(bad.clone().into_boxed_slice(), ReadLimits::default()).is_err());
        assert_eq!(
            recovered_prefix(&bad, &original).unwrap(),
            original.entries().len()
        );
    }
}

/// Members whose stream range overlaps the data frame at file offset `at`.
fn touched_by(original: &Archive<[u8]>, frame: &zalt_archive::read::Frame) -> Vec<Vec<u8>> {
    let (s, t) = (
        frame.stream_offset,
        frame.stream_offset + frame.decompressed_size,
    );
    original
        .entries()
        .iter()
        .filter(|e| e.header_offset < t && e.data_offset + e.size.div_ceil(512) * 512 > s)
        .map(|e| e.path.clone())
        .collect()
}

/// Recover `bytes`, check the result verifies and every member in it is one
/// of `original`'s with the same content, and return the recovered paths.
fn salvaged(bytes: &[u8], original: &Archive<[u8]>) -> (Vec<Vec<u8>>, zalt_archive::Recovered) {
    let (fixed, report) = recover(bytes, Vec::new(), options()).unwrap();
    let a = open(&fixed);
    a.verify().unwrap();
    let mut paths = Vec::new();
    for e in a.entries() {
        let o = original.find(&e.path).expect("a member the original had");
        assert_eq!(a.entry_bytes(e).unwrap(), original.entry_bytes(o).unwrap());
        paths.push(e.path.clone());
    }
    assert_eq!(report.entries as usize, paths.len());
    (paths, report)
}

/// A flipped byte anywhere in any data frame, the trailer intact: every
/// member the damaged frame does not touch comes back, byte for byte — the
/// members *after* the damage included — and when the frame is reported
/// damaged, exactly the members it touches are lost.
#[test]
fn damage_costs_only_the_members_it_touches() {
    let bytes = write(&first_batch());
    let original = open(&bytes);
    let all: Vec<Vec<u8>> = original.entries().iter().map(|e| e.path.clone()).collect();
    let data: Vec<_> = original
        .frames()
        .iter()
        .filter(|f| f.is_data())
        .copied()
        .collect();
    let mut flips = 0;
    for f in &data[..data.len() - 1] {
        for at in (f.file_offset..f.file_offset + f.compressed_size).step_by(7) {
            let mut bad = bytes.clone();
            bad[at as usize] ^= 0x10;
            let (kept, report) = salvaged(&bad, &original);
            let touched = touched_by(&original, f);
            let untouched: Vec<_> = all.iter().filter(|p| !touched.contains(p)).collect();
            for p in &untouched {
                assert!(
                    kept.contains(p),
                    "byte {at}: lost {}",
                    String::from_utf8_lossy(p)
                );
            }
            if report.damaged_frames == 1 {
                assert_eq!(kept.len(), untouched.len(), "byte {at}");
                assert_eq!(report.lost, touched, "byte {at}");
            } else {
                // A bit the decoder may ignore (the descriptor's Unused_bit).
                assert_eq!(report.damaged_frames, 0, "byte {at}");
                assert_eq!(kept, all, "byte {at}");
            }
            flips += 1;
        }
    }
    assert!(flips > 200, "{flips}");
}

/// The frame's own block headers damaged, so its length cannot be read from
/// it: the seek table still says where the next frame starts.
#[test]
fn the_seek_table_bridges_a_frame_whose_length_was_lost() {
    let bytes = write(&first_batch());
    let original = open(&bytes);
    let data: Vec<_> = original
        .frames()
        .iter()
        .filter(|f| f.is_data())
        .copied()
        .collect();
    let victim = data[1];
    let mut bad = bytes.clone();
    // The block header right after a single-segment header with a 2-byte
    // content size: turn its type into the reserved one.
    let block = (victim.file_offset + 4 + 1 + 2) as usize;
    bad[block] |= 0b110;
    assert!(zalt_codec::frame::compressed_size(&bad[victim.file_offset as usize..]).is_err());
    let (kept, report) = salvaged(&bad, &original);
    assert_eq!(report.damaged_frames, 1);
    assert_eq!(
        kept.len(),
        original.entries().len() - touched_by(&original, &victim).len()
    );
}

/// Trailer gone too. Damage inside one member's content still costs only that
/// member: its header, read before the damage, says where the next begins.
#[test]
fn without_a_trailer_damage_inside_a_file_costs_only_that_file() {
    let bytes = write(&first_batch());
    let original = open(&bytes);
    let large = original.find(b"a/large.bin").unwrap();
    let chunks: Vec<_> = original
        .frames()
        .iter()
        .filter(|f| f.is_data() && f.stream_offset > large.data_offset)
        .filter(|f| f.stream_offset + f.decompressed_size < large.data_offset + large.size)
        .copied()
        .collect();
    let victim = chunks[0];
    let index = original.frames()[original.frames().len() - 1].file_offset as usize;
    let mut bad = bytes[..index].to_vec();
    bad[(victim.file_offset + victim.compressed_size / 2) as usize] ^= 0xff;
    let (kept, report) = salvaged(&bad, &original);
    assert_eq!(report.damaged_frames, 1);
    assert_eq!(report.lost, vec![b"a/large.bin".to_vec()]);
    assert!(
        kept.contains(&b"a/three.txt".to_vec()),
        "the member after it"
    );
    assert!(report.terminator_found);
}

/// Trailer gone, and the damage took member headers with it: with no index
/// to say where the next member starts, the walk stops rather than guess.
/// With the index, it picks the thread up again.
#[test]
fn lost_headers_stop_the_walk_unless_the_index_survived() {
    let bytes = write(&first_batch());
    let original = open(&bytes);
    let data: Vec<_> = original
        .frames()
        .iter()
        .filter(|f| f.is_data())
        .copied()
        .collect();
    // The first frame: the solid group holding the first members' headers.
    let victim = data[0];
    let flip = (victim.file_offset + victim.compressed_size / 2) as usize;
    let index = original.frames()[original.frames().len() - 1].file_offset as usize;
    let touched = touched_by(&original, &victim);

    let mut no_trailer = bytes[..index].to_vec();
    no_trailer[flip] ^= 0xff;
    let (kept, report) = salvaged(&no_trailer, &original);
    assert!(kept.is_empty(), "{kept:?}");
    assert!(report.stopped.is_some() && !report.resynced_by_index);

    // Seek table damaged, index intact: found by walking, used to resync.
    let mut with_index = bytes.clone();
    with_index[flip] ^= 0xff;
    let last = with_index.len() - 1;
    with_index[last] ^= 0xff;
    let (kept, report) = salvaged(&with_index, &original);
    assert!(report.resynced_by_index, "{report:?}");
    assert_eq!(kept.len(), original.entries().len() - touched.len());
}

#[test]
fn a_crash_mid_append_loses_only_the_unfinished_members() {
    let before = write(&first_batch());
    let after = append_to(&before, &second_batch(), options());
    let complete = open(&after);
    let old = open(&before).entries().len();
    let start = terminator_offset(&before) as usize;
    let end = complete.frames()[complete.frames().len() - 1].file_offset as usize;
    for cut in (start..=end).step_by(37).chain([start, end]) {
        let k = recovered_prefix(&after[..cut], &complete).unwrap();
        assert!(k >= old, "cut {cut}: lost an old member ({k} < {old})");
    }
}

/// §11.3 says the Terminator is "the last data frame that decodes to only
/// zero bytes". A middle chunk of an all-zero file is one too: taken at its
/// word, recovery of an archive that lost its Terminator would cut that file
/// in half. Following the tar stream, it does not.
#[test]
fn an_all_zero_chunk_is_not_mistaken_for_the_terminator() {
    let members = vec![
        file("zeros.bin", vec![0u8; 20_000]),
        file("after.txt", b"after\n".to_vec()),
    ];
    let bytes = write(&members);
    let original = open(&bytes);
    let zero_frames = original
        .frames()
        .iter()
        .filter(|f| f.is_data() && f.decompressed_size >= 1024)
        .filter(|f| {
            original
                .read_range(f.stream_offset, f.stream_offset + f.decompressed_size)
                .unwrap()
                .iter()
                .all(|&b| b == 0)
        })
        .count();
    assert!(
        zero_frames >= 3,
        "the file's middle chunks and the Terminator"
    );
    let terminator = terminator_offset(&bytes) as usize;
    assert_eq!(
        recovered_prefix(&bytes[..terminator], &original).unwrap(),
        2
    );
}

#[test]
fn what_is_not_an_archive_cannot_be_recovered() {
    for junk in [
        &b""[..],
        b"\x28\xb5\x2f\xfd",
        &[0u8; 64],
        b"PK\x03\x04 a zip file",
    ] {
        assert!(recover(junk, Vec::new(), options()).is_err());
    }
}

/// A frame that decodes perfectly but holds a bad tar header — damage that
/// happened before compression, or a writer's bug. The run it is in cannot be
/// accounted for member by member, so it is dropped whole; keeping it would
/// leave tar bytes no index record describes. Built with the probe's writer,
/// since ours cannot write a bad header.
#[test]
fn a_bad_tar_header_inside_a_good_frame_drops_its_run() {
    let mut t = zsta_seekable_probe::tar::TarWriter::new();
    for i in 0..12 {
        t.add_file(
            &format!("lib/f{i:02}"),
            &pseudo(700, i),
            0o644,
            1_700_000_000,
        );
    }
    let (mut body, entries) = t.finish();
    // Two files to a frame at this solid target: the bad header is the
    // second in its frame, so the run has one good member before it — the
    // case where only checking that members tile the run tells it apart.
    let victim = &entries[7];
    // The ustar block's first name byte: its checksum no longer matches.
    body[(victim.data_offset - 512) as usize] ^= 0x20;
    let opts = zsta_seekable_probe::archive::WriterOptions {
        solid_target: 4096,
        chunk: 8192,
        ..Default::default()
    };
    let mut enc = zsta_seekable_probe::encoder::ReferenceEncoder::new(3);
    let bytes = zsta_seekable_probe::archive::write(&body, &entries, &mut enc, &opts)
        .unwrap()
        .bytes;
    let original = open(&bytes);
    let (kept, report) = salvaged(&bytes, &original);
    assert_eq!(report.damaged_frames, 0, "every frame decodes");
    for lost in [&b"lib/f06"[..], b"lib/f07"] {
        assert!(!kept.contains(&lost.to_vec()));
        assert!(report.lost.contains(&lost.to_vec()), "{report:?}");
    }
    assert!(
        kept.contains(&b"lib/f00".to_vec()),
        "runs before it are kept"
    );
    assert!(
        kept.contains(&b"lib/f11".to_vec()),
        "and after it, through the index"
    );
}

// ------------------------------------------------------------------ search

fn search() -> zalt_archive::RecoverOptions {
    zalt_archive::RecoverOptions { search: true }
}

/// Headers lost, no trailer, no index: without search the walk stops; with
/// it, the walk resumes at the next frame that starts with a tar header, and
/// says which members it found that way.
#[test]
fn search_resumes_where_there_is_no_index_and_says_what_it_found() {
    let bytes = write(&first_batch());
    let original = open(&bytes);
    let data: Vec<_> = original
        .frames()
        .iter()
        .filter(|f| f.is_data())
        .copied()
        .collect();
    let victim = data[0];
    let index = original.frames()[original.frames().len() - 1].file_offset as usize;
    let mut bad = bytes[..index].to_vec();
    bad[(victim.file_offset + victim.compressed_size / 2) as usize] ^= 0xff;
    let touched = touched_by(&original, &victim);

    let (kept, _) = salvaged(&bad, &original);
    assert!(kept.is_empty());

    let (fixed, report) =
        zalt_archive::recover_with(bad.as_slice(), Vec::new(), options(), &search()).unwrap();
    let a = open(&fixed);
    a.verify().unwrap();
    let kept: Vec<_> = a.entries().iter().map(|e| e.path.clone()).collect();
    assert_eq!(kept.len(), original.entries().len() - touched.len());
    for e in a.entries() {
        let o = original.find(&e.path).unwrap();
        assert_eq!(a.entry_bytes(e).unwrap(), original.entry_bytes(o).unwrap());
    }
    assert_eq!(
        report.found_by_search, kept,
        "everything after the damage was a guess"
    );
}

/// Why search is opt-in. An archived `.tar` has valid tar headers in its
/// content; when the member holding it loses its own header, search takes
/// up the thread *inside* it, and the inner tar's files come back as if they
/// were the archive's. They are always in `found_by_search`, so a caller can
/// tell — this test pins that, and pins that without search they never
/// appear.
#[test]
fn search_can_be_fooled_by_an_archived_tar_and_reports_what_it_found() {
    // An inner tar of empty files: one header block each, so every block is
    // a header and every chunk of it starts with one.
    let mut inner = Vec::new();
    for i in 0..40 {
        let e = Entry::new(Kind::File, format!("inner/e{i:02}").into_bytes());
        zalt_archive::tar::write_headers(&mut inner, &e, &Default::default()).unwrap();
    }
    inner.extend_from_slice(&[0u8; 1024]);
    let members = vec![
        file("x.txt", b"before\n".to_vec()),
        file("pkg/inner.tar", inner),
    ];
    let bytes = write(&members);
    let original = open(&bytes);
    let tarball = original.find(b"pkg/inner.tar").unwrap();
    // Damage the frame holding the inner tar's own header.
    let first_chunk = *original
        .frames()
        .iter()
        .find(|f| f.is_data() && f.stream_offset == tarball.header_offset)
        .unwrap();
    let index = original.frames()[original.frames().len() - 1].file_offset as usize;
    let mut bad = bytes[..index].to_vec();
    bad[(first_chunk.file_offset + first_chunk.compressed_size / 2) as usize] ^= 0xff;

    let (fixed, _) = recover(bad.as_slice(), Vec::new(), options()).unwrap();
    let plain: Vec<_> = open(&fixed)
        .entries()
        .iter()
        .map(|e| e.path.clone())
        .collect();
    assert_eq!(
        plain,
        vec![b"x.txt".to_vec()],
        "without search: nothing invented"
    );

    let (fixed, report) =
        zalt_archive::recover_with(bad.as_slice(), Vec::new(), options(), &search()).unwrap();
    let a = open(&fixed);
    a.verify().unwrap();
    let invented: Vec<_> = a
        .entries()
        .iter()
        .filter(|e| e.path.starts_with(b"inner/"))
        .map(|e| e.path.clone())
        .collect();
    assert!(!invented.is_empty(), "the hazard this test exists to show");
    for p in &invented {
        assert!(
            report.found_by_search.contains(p),
            "{}",
            String::from_utf8_lossy(p)
        );
    }
}
