//! Write an archive in memory, read it back, and break it on purpose.

use zalt_archive::index::{Index, IndexLimits};
use zalt_archive::layout::{self, MAGIC_HEADER, MAGIC_INDEX, SeekEntry};
use zalt_archive::read::Archive;
use zalt_archive::tar;
use zalt_archive::{Entry, Error, FramePolicy, Kind, ReadLimits, Writer, WriterOptions};

/// A policy scaled down so a few kilobytes exercise every placement rule.
///
/// The small-file threshold has to clear the headers, not only the content:
/// every member here has a sub-second mtime, so a pax header — three blocks
/// with the ustar one — and a 110-byte file is already 2 KiB of tar.
fn tiny_policy() -> FramePolicy {
    FramePolicy {
        small_file_threshold: 4096,
        solid_target: 8192,
        large_chunk: 4096,
    }
}

fn options(threads: usize) -> WriterOptions {
    WriterOptions {
        level: 3,
        policy: tiny_policy(),
        threads,
        creator: Some(b"test".to_vec()),
    }
}

fn pseudo(n: usize, seed: u32) -> Vec<u8> {
    let mut s = seed;
    (0..n)
        .map(|i| {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            if i % 3 == 0 {
                b'a' + (s >> 28) as u8
            } else {
                (s >> 24) as u8
            }
        })
        .collect()
}

/// Members of every kind, and every placement: small (solid), medium (own
/// frame), large (chunked), empty, long path, decomposed Unicode, links.
fn members() -> Vec<(Entry, Vec<u8>)> {
    let file = |path: &str, content: Vec<u8>| {
        let mut e = Entry::new(Kind::File, path.as_bytes());
        e.size = content.len() as u64;
        e.mtime_sec = 1_700_000_000;
        e.mtime_nsec = 123_456_789;
        (e, content)
    };
    let mut dir = Entry::new(Kind::Directory, "proj");
    dir.mode = 0o755;
    let mut link = Entry::new(Kind::Symlink, "proj/latest");
    link.link = Some(b"small_1.txt".to_vec());
    let mut hard = Entry::new(Kind::HardLink, "proj/copy_of_small_0");
    hard.link = Some(b"proj/small_0.txt".to_vec());
    let long = format!("proj/{}/end.txt", "deep".repeat(40));
    vec![
        (dir, vec![]),
        file("proj/small_0.txt", b"hello zsta\n".repeat(10)),
        file("proj/small_1.txt", pseudo(900, 1)),
        file("proj/small_2.txt", pseudo(1500, 2)),
        file("proj/empty.txt", vec![]),
        file("proj/medium.bin", pseudo(6000, 3)),
        file("proj/large.bin", pseudo(20_000, 4)),
        (link, vec![]),
        (hard, vec![]),
        file(&long, b"far down\n".to_vec()),
        file(
            "proj/donne\u{301}es/re\u{301}sume\u{301}.txt",
            b"NFD, kept as given\n".to_vec(),
        ),
    ]
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

#[test]
fn every_member_comes_back_exactly() {
    let members = members();
    let archive = open(write(&members, options(4))).unwrap();
    archive.verify().unwrap();
    assert_eq!(archive.entries().len(), members.len());
    for ((want, content), got) in members.iter().zip(archive.entries()) {
        assert_eq!(got.path, want.path);
        assert_eq!(got.kind, want.kind);
        assert_eq!(got.size, content.len() as u64);
        assert_eq!(got.link, want.link);
        assert_eq!(
            (got.mtime_sec, got.mtime_nsec),
            (want.mtime_sec, want.mtime_nsec)
        );
        assert_eq!(got.mode, want.mode);
        assert_eq!(
            &archive.entry_bytes(got).unwrap(),
            content,
            "{}",
            got.path_lossy()
        );
    }
    assert_eq!(archive.header().creator(), Some(&b"test"[..]));
}

#[test]
fn the_layout_is_the_one_the_policy_promises() {
    let members = members();
    let archive = open(write(&members, options(4))).unwrap();
    let frames = archive.frames();
    assert_eq!(frames[0].magic, MAGIC_HEADER);
    assert_eq!(frames[frames.len() - 1].magic, MAGIC_INDEX);
    let data: Vec<_> = frames.iter().filter(|f| f.is_data()).collect();
    // The Terminator: last data frame, exactly 1024 bytes.
    assert_eq!(data.last().unwrap().decompressed_size, 1024);
    for f in &data {
        assert_eq!(f.decompressed_size % 512, 0, "whole tar blocks");
        assert!(f.decompressed_size <= 8192, "nothing over the solid target");
    }
    let frame_of = |offset: u64| {
        data.iter()
            .position(|f| {
                f.stream_offset <= offset && offset < f.stream_offset + f.decompressed_size
            })
            .unwrap()
    };
    let by_path = |p: &str| archive.find(p.as_bytes()).unwrap();
    // Small files share a solid frame.
    assert_eq!(
        frame_of(by_path("proj/small_0.txt").header_offset),
        frame_of(by_path("proj/small_1.txt").header_offset)
    );
    // A medium file sits in a frame of its own, whole.
    let medium = by_path("proj/medium.bin");
    let f = frame_of(medium.header_offset);
    assert_eq!(
        f,
        frame_of(medium.data_offset + medium.size - 1),
        "not split"
    );
    assert_eq!(
        data[f].stream_offset, medium.header_offset,
        "starts its frame"
    );
    // A large file is split across chunks.
    let large = by_path("proj/large.bin");
    assert!(frame_of(large.data_offset + large.size - 1) > frame_of(large.header_offset) + 3);
}

/// §12.4, and the claim the parallel writer rests on: frames are compressed
/// independently, so the thread count cannot change a single byte.
#[test]
fn output_does_not_depend_on_the_thread_count() {
    let members = members();
    let one = write(&members, options(1));
    for threads in [2, 3, 8] {
        assert_eq!(write(&members, options(threads)), one, "{threads} threads");
    }
}

/// What `zstd -dc` sees: every frame, skippable ones stepped over, data frames
/// concatenated — a tar stream whose members are the archive's.
#[test]
fn a_stock_decoder_sees_a_tar_stream() {
    let members = members();
    let bytes = write(&members, options(4));
    let stream = zalt_codec::decompress(&bytes, Default::default()).unwrap();
    let archive = open(bytes).unwrap();
    assert_eq!(stream.len() as u64, archive.tar_stream_size());
    let mut pos = 0usize;
    for (want, content) in &members {
        let m = tar::parse_member(&stream[pos..]).unwrap();
        assert_eq!(m.path, want.path);
        let start = pos + m.header_len as usize;
        assert_eq!(&stream[start..start + content.len()], content.as_slice());
        pos = start + tar::pad(m.size) as usize;
    }
    assert!(
        stream[pos..].iter().all(|&b| b == 0),
        "only the Terminator remains"
    );
    assert_eq!(stream.len() - pos, 1024);
}

#[test]
fn an_empty_archive_is_header_terminator_index_and_seek_table() {
    let archive = open(write(&[], options(1))).unwrap();
    archive.verify().unwrap();
    assert!(archive.entries().is_empty());
    assert_eq!(archive.frames().len(), 3);
    assert_eq!(archive.tar_stream_size(), 1024);
}

#[test]
fn a_corrupted_data_frame_is_caught_where_it_is_read() {
    let members = members();
    let mut bytes = write(&members, options(4));
    let archive = open(bytes.clone()).unwrap();
    let small = archive.find(b"proj/small_1.txt").unwrap().clone();
    let frame = *archive
        .frames()
        .iter()
        .find(|f| {
            f.is_data()
                && f.stream_offset <= small.data_offset
                && small.data_offset < f.stream_offset + f.decompressed_size
        })
        .unwrap();
    // Flip a byte in the middle of that frame's compressed payload.
    bytes[(frame.file_offset + frame.compressed_size / 2) as usize] ^= 0x40;
    let damaged = open(bytes).unwrap();
    assert!(
        damaged.entry_bytes(&small).is_err(),
        "decode or checksum must fail"
    );
    assert!(damaged.verify().is_err());
    // An entry in another frame is still readable: damage stays local.
    let medium = damaged.find(b"proj/medium.bin").unwrap().clone();
    assert!(damaged.entry_bytes(&medium).is_ok());
}

#[test]
fn a_truncated_or_padded_file_will_not_open() {
    let bytes = write(&members(), options(4));
    assert!(open(bytes[..bytes.len() - 1].to_vec()).is_err());
    let mut padded = vec![0u8];
    padded.extend_from_slice(&bytes);
    assert!(
        open(padded).is_err(),
        "§11.1 step 4: the table must cover the file"
    );
}

/// Replace an archive's Central Index, re-sizing the seek table to match, so a
/// doctored index reaches the reader's checks rather than its parser's.
fn with_index(bytes: &[u8], doctor: impl FnOnce(&mut Index)) -> Vec<u8> {
    let archive = open(bytes.to_vec()).unwrap();
    let frames = archive.frames().to_vec();
    let last = frames[frames.len() - 1];
    let payload = layout::skippable_payload(
        &bytes[last.file_offset as usize..(last.file_offset + last.compressed_size) as usize],
        MAGIC_INDEX,
    )
    .unwrap();
    let mut index = Index::parse(payload, &IndexLimits::default()).unwrap();
    doctor(&mut index);
    let new_index = layout::skippable(MAGIC_INDEX, &index.encode(3).unwrap()).unwrap();

    let mut out = bytes[..last.file_offset as usize].to_vec();
    out.extend_from_slice(&new_index);
    let mut seek: Vec<SeekEntry> = frames
        .iter()
        .map(|f| SeekEntry {
            compressed_size: f.compressed_size as u32,
            decompressed_size: f.decompressed_size as u32,
            checksum: f.checksum,
        })
        .collect();
    seek.last_mut().unwrap().compressed_size = new_index.len() as u32;
    out.extend_from_slice(&layout::encode_seek_table(&seek).unwrap());
    out
}

/// A member present in the tar stream but missing from the index opens and
/// lists without complaint — the index is self-consistent — and `verify`
/// catches it, because the remaining members no longer tile the stream.
/// A stock `tar -x` would extract the hidden file; an indexed reader that
/// trusted the index would never mention it.
#[test]
fn a_member_hidden_from_the_index_is_found_by_verify() {
    let bytes = write(&members(), options(4));
    let doctored = with_index(&bytes, |idx| {
        idx.entries.retain(|e| e.path != b"proj/small_1.txt");
    });
    let archive = open(doctored).expect("the index alone is consistent");
    assert_eq!(archive.entries().len(), members().len() - 1);
    assert!(matches!(archive.verify(), Err(Error::Inconsistent(_))));
}

#[test]
fn an_index_that_lies_about_a_member_is_caught_on_read() {
    let bytes = write(&members(), options(4));
    let doctored = with_index(&bytes, |idx| {
        let e = idx
            .entries
            .iter_mut()
            .find(|e| e.path == b"proj/small_2.txt")
            .unwrap();
        e.path = b"proj/renamed.txt".to_vec();
    });
    let archive = open(doctored).unwrap();
    let e = archive.find(b"proj/renamed.txt").unwrap().clone();
    assert!(matches!(
        archive.entry_bytes(&e),
        Err(Error::Inconsistent(_))
    ));
}

#[test]
fn the_stream_size_must_be_the_data_frames_total() {
    let bytes = write(&members(), options(4));
    let doctored = with_index(&bytes, |idx| idx.tar_stream_size += 512);
    assert!(open(doctored).is_err());
}

#[test]
fn a_policy_that_cannot_describe_a_valid_archive_is_refused() {
    for policy in [
        FramePolicy {
            large_chunk: 1000,
            ..FramePolicy::default()
        },
        FramePolicy {
            small_file_threshold: 8 << 20,
            ..FramePolicy::default()
        },
        FramePolicy {
            large_chunk: 2 << 30,
            ..FramePolicy::default()
        },
    ] {
        let o = WriterOptions {
            policy,
            ..WriterOptions::default()
        };
        assert!(Writer::new(Vec::new(), o).is_err(), "{policy:?}");
    }
}
