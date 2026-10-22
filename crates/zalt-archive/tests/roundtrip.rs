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
        hash: zalt_archive::Hash::default(),
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
        file("proj/small_0.txt", b"hello zlta\n".repeat(10)),
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
            small_file_threshold: FramePolicy::default().solid_target + 512,
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

/// §7.2 makes the metadata fields optional and §7.5 makes the index a cache:
/// an index that leaves one out describes the member no less truly, and its
/// members must still read. One that records a *different* value is wrong.
/// (Found by the oracle: the probe's writer omits `NAMES` while its tar
/// headers carry uname/gname, and a strict comparison refused every member.)
#[test]
fn optional_index_fields_may_be_omitted_but_not_contradict_the_tar_headers() {
    let mut e = Entry::new(Kind::File, "f");
    e.size = 3;
    e.names = Some((b"dan".to_vec(), b"staff".to_vec()));
    e.btime = Some((1_600_000_000, 5));
    e.fflags = Some(0x8000);
    let mut w = Writer::new(Vec::new(), options(1)).unwrap();
    w.add(e, &mut &b"abc"[..]).unwrap();
    let bytes = w.finish().unwrap().0;

    let omitted = with_index(&bytes, |idx| {
        let e = &mut idx.entries[0];
        (e.names, e.btime, e.fflags) = (None, None, None);
    });
    let a = open(omitted).unwrap();
    assert_eq!(a.entry_bytes(&a.entries()[0]).unwrap(), b"abc");

    let doctors: [fn(&mut Entry); 3] = [
        |e| e.names = Some((b"eve".to_vec(), b"staff".to_vec())),
        |e| e.btime = Some((1, 0)),
        |e| e.fflags = Some(0x2),
    ];
    for doctor in doctors {
        let a = open(with_index(&bytes, |idx| doctor(&mut idx.entries[0]))).unwrap();
        assert!(matches!(
            a.entry_bytes(&a.entries()[0]),
            Err(Error::Inconsistent(_))
        ));
    }
}

/// Reading a whole archive should decode each frame about once. A solid
/// group holds many members; decoding the group again for every one of them
/// — once for its headers, once for its content — costs members × group size,
/// which on a real tree is hundreds of times the archive.
#[test]
fn reading_everything_decodes_each_frame_about_once() {
    let mut w = Writer::new(Vec::new(), WriterOptions::default()).unwrap();
    for i in 0..400 {
        let mut e = Entry::new(Kind::File, format!("many/f{i:04}").into_bytes());
        let content = pseudo(900, i);
        e.size = content.len() as u64;
        w.add(e, &mut content.as_slice()).unwrap();
    }
    let bytes = w.finish().unwrap().0;
    let a = open(bytes.clone()).unwrap();
    let stream = a.tar_stream_size();
    a.verify().unwrap();
    let verify = a.bytes_decoded();
    // Each measured on a freshly opened archive, so neither rides on the
    // other's decoding.
    let b = open(bytes).unwrap();
    for e in b.entries() {
        b.entry_bytes(e).unwrap();
    }
    let extract = b.bytes_decoded();
    eprintln!(
        "tar stream {stream} B: verify decoded {:.1}x, reading every member {:.1}x",
        verify as f64 / stream as f64,
        extract as f64 / stream as f64
    );
    assert!(verify <= 3 * stream, "verify decoded {verify} for {stream}");
    assert!(
        extract <= 3 * stream,
        "reading every member decoded {extract} for {stream}"
    );
}

/// Counts what is written to it and checks it is all zeros, holding nothing.
struct ZeroSink(u64);
impl std::io::Write for ZeroSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        assert!(
            buf.iter().all(|&b| b == 0),
            "content that should be zeros is not"
        );
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Members past 4 GiB (32-bit sizes) and past 8 GiB (tar's octal size field;
/// §9.1 requires a pax `size` record), written and read back whole — in
/// memory, since zeros compress to almost nothing, and checked through a sink
/// so nothing that size is ever held. Ignored by default: it decodes ~14 GiB.
/// `just archive-huge` runs it.
#[test]
#[ignore = "encodes and decodes ~14 GiB; run with `just archive-huge`"]
fn members_past_4_and_8_gib_round_trip() {
    use std::io::Read;
    for size in [(5u64 << 30) + 123, (9u64 << 30) + 3 * 512 + 7] {
        let options = WriterOptions {
            level: 1,
            threads: 2,
            ..WriterOptions::default()
        };
        let mut w = Writer::new(Vec::new(), options).unwrap();
        let mut huge = Entry::new(Kind::File, "huge.bin");
        huge.size = size;
        w.add(huge, &mut std::io::repeat(0).take(size)).unwrap();
        let mut after = Entry::new(Kind::File, "after.txt");
        after.size = 6;
        w.add(after, &mut &b"after\n"[..]).unwrap();
        let bytes = w.finish().unwrap().0;

        let a = open(bytes).unwrap();
        let e = a.find(b"huge.bin").unwrap().clone();
        assert_eq!(e.size, size);
        let mut sink = ZeroSink(0);
        let member = a.copy_entry(&e, &mut sink).unwrap();
        assert_eq!((sink.0, member.size), (size, size));
        if size >= 8 << 30 {
            assert!(
                member.header_len > 512,
                "past 8 GiB the size needs a pax record"
            );
        }
        let after = a.find(b"after.txt").unwrap();
        assert_eq!(a.entry_bytes(after).unwrap(), b"after\n");
        eprintln!(
            "{size} B member: archive {} B, {} frames",
            a.frames().iter().map(|f| f.compressed_size).sum::<u64>(),
            a.frames().len()
        );
    }
}

/// The same, for an archive many times larger than the frame cache — the
/// case the first version of the test above missed, because its archive fit
/// in the cache whole. `verify` then decoded every frame in a parallel pass
/// and *again*, one thread, to check headers: 2x, found by `just
/// archive-bench` on a 578 MB tree as 0.30x of `zstd -t`.
#[test]
fn verify_decodes_each_frame_once_when_the_archive_outgrows_the_cache() {
    let options = WriterOptions {
        policy: FramePolicy {
            small_file_threshold: 32 << 10,
            solid_target: 64 << 10,
            large_chunk: 64 << 10,
        },
        ..WriterOptions::default()
    };
    let mut w = Writer::new(Vec::new(), options).unwrap();
    for i in 0..600 {
        let mut e = Entry::new(Kind::File, format!("many/f{i:04}").into_bytes());
        let content = pseudo(1500 + (i as usize % 7) * 3000, i);
        e.size = content.len() as u64;
        w.add(e, &mut content.as_slice()).unwrap();
    }
    let bytes = w.finish().unwrap().0;
    let limits = ReadLimits {
        cache_bytes: 160 << 10, // two or three frames
        ..ReadLimits::default()
    };
    let a = Archive::open(bytes.into_boxed_slice(), limits).unwrap();
    let stream = a.tar_stream_size();
    assert!(
        stream > 20 * limits.cache_bytes,
        "the point is an archive that does not fit"
    );
    a.verify().unwrap();
    let ratio = a.bytes_decoded() as f64 / stream as f64;
    eprintln!(
        "verify decoded {ratio:.2}x of a {stream} B stream with a {} B cache",
        limits.cache_bytes
    );
    assert!(ratio <= 1.1, "verify decoded {ratio:.2}x");
}

// ------------------------------------------------------------ entry hashes

/// The BLAKE3 the crate links is BLAKE3: the official test vector for empty
/// input, so a build that quietly hashed something else would fail here.
#[test]
fn blake3_is_blake3() {
    assert_eq!(
        blake3::hash(b"").to_hex().as_str(),
        "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
    );
}

/// By default every member with content carries the BLAKE3 of it — an empty
/// file included — and nothing else carries a hash.
#[test]
fn every_member_carries_the_blake3_of_its_content_by_default() {
    let members = members();
    let a = open(write(&members, options(2))).unwrap();
    assert_eq!(a.hash_algorithm(), zalt_archive::hash::algorithm::BLAKE3);
    assert!(a.checks_hashes());
    for (want, content) in &members {
        let e = a.entries().iter().find(|e| e.path == want.path).unwrap();
        if e.kind == Kind::File {
            let digest = blake3::hash(content);
            assert_eq!(
                e.hash.as_deref(),
                Some(digest.as_bytes().as_slice()),
                "{}",
                e.path_lossy()
            );
        } else {
            assert_eq!(e.hash, None, "{}", e.path_lossy());
        }
    }
    a.verify().unwrap();
}

/// XXH3-128 when asked, in the canonical byte order, matching xxHash's own C.
#[test]
fn xxh3_hashes_match_the_reference_implementation() {
    let members = members();
    let o = WriterOptions {
        hash: zalt_archive::Hash::Xxh3,
        ..options(2)
    };
    let a = open(write(&members, o)).unwrap();
    assert_eq!(a.hash_algorithm(), zalt_archive::hash::algorithm::XXH3_128);
    for (want, content) in &members {
        if want.kind != Kind::File {
            continue;
        }
        let e = a.entries().iter().find(|e| e.path == want.path).unwrap();
        let reference = zlta_seekable_probe::xxh3::xxh3_128(content);
        assert_eq!(
            e.hash.as_deref(),
            Some(reference.as_slice()),
            "{}",
            e.path_lossy()
        );
    }
    a.verify().unwrap();
}

/// `Hash::None` writes `Hash_Algorithm` 0 and no digests, and still verifies.
#[test]
fn an_unhashed_archive_has_no_digests_and_still_verifies() {
    let o = WriterOptions {
        hash: zalt_archive::Hash::None,
        ..options(2)
    };
    let a = open(write(&members(), o)).unwrap();
    assert_eq!(a.hash_algorithm(), 0);
    assert!(!a.checks_hashes());
    assert!(a.entries().iter().all(|e| e.hash.is_none()));
    a.verify().unwrap();
}

/// Our XXH3-128 against the reference on every length to 2200 — all four
/// short paths, then one, two and three 1024-byte blocks with every stripe
/// remainder — and on longer inputs, whole and fed in pieces of every size
/// that matters to the streaming buffer.
#[test]
fn xxh3_matches_the_reference_on_every_length_and_split() {
    use zalt_archive::xxh3;
    let data = pseudo(200_000, 9);
    for len in 0..=2200 {
        let reference = zlta_seekable_probe::xxh3::xxh3_128(&data[..len]);
        assert_eq!(
            xxh3::canonical(xxh3::hash(&data[..len])),
            reference,
            "len {len}"
        );
    }
    for len in [
        240, 241, 255, 256, 257, 4095, 4096, 4097, 16_383, 16_384, 16_385, 100_003, 200_000,
    ] {
        let reference = zlta_seekable_probe::xxh3::xxh3_128(&data[..len]);
        assert_eq!(
            xxh3::canonical(xxh3::hash(&data[..len])),
            reference,
            "len {len}"
        );
        for piece in [1usize, 3, 63, 64, 65, 192, 255, 256, 257, 1000, 4096] {
            let mut s = xxh3::Xxh3::new();
            for chunk in data[..len].chunks(piece) {
                s.update(chunk);
            }
            assert_eq!(
                xxh3::canonical(s.digest()),
                reference,
                "len {len}, pieces of {piece}"
            );
        }
    }
}

/// Content that does not match its hash — every frame checksum intact, so
/// only the hash can tell — is refused by `verify` and by reading the member,
/// and the other members still read. Doctored through the index, the one
/// place a digest lives.
#[test]
fn content_that_does_not_match_its_hash_is_refused() {
    for target in ["proj/small_1.txt", "proj/large.bin", "proj/empty.txt"] {
        let bytes = write(&members(), options(2));
        let doctored = with_index(&bytes, |index| {
            let e = index
                .entries
                .iter_mut()
                .find(|e| e.path == target.as_bytes())
                .unwrap();
            e.hash.as_mut().unwrap()[5] ^= 1;
        });
        let a = open(doctored).unwrap();
        let err = a.verify().unwrap_err().to_string();
        assert!(
            err.contains(target) && err.contains("BLAKE3"),
            "{target}: {err}"
        );
        for e in a.entries() {
            let read = a.entry_bytes(e);
            if e.path == target.as_bytes() {
                assert!(read.is_err(), "{target} read despite its hash");
            } else {
                read.unwrap();
            }
        }
    }
}

/// A member far larger than `verify`'s window is hashed across windows — fed
/// as each one is decoded, not read again — so a wrong digest at its end is
/// still caught, and nothing is decoded twice.
#[test]
fn a_member_larger_than_the_cache_is_hashed_across_windows_and_decoded_once() {
    let options = WriterOptions {
        policy: FramePolicy {
            small_file_threshold: 32 << 10,
            solid_target: 64 << 10,
            large_chunk: 64 << 10,
        },
        ..WriterOptions::default()
    };
    let big = pseudo(3 << 20, 77);
    let mut members = vec![];
    for i in 0..20 {
        let content = pseudo(5000 + i * 300, i as u32);
        let mut e = Entry::new(Kind::File, format!("before/{i:02}").into_bytes());
        e.size = content.len() as u64;
        members.push((e, content));
    }
    let mut e = Entry::new(Kind::File, "big.bin");
    e.size = big.len() as u64;
    members.push((e, big));
    let mut e = Entry::new(Kind::File, "after.txt");
    e.size = 5;
    members.push((e, b"after".to_vec()));
    let bytes = write(&members, options);
    let limits = ReadLimits {
        cache_bytes: 160 << 10,
        ..ReadLimits::default()
    };

    let a = Archive::open(bytes.clone().into_boxed_slice(), limits).unwrap();
    a.verify().unwrap();
    let ratio = a.bytes_decoded() as f64 / a.tar_stream_size() as f64;
    assert!(ratio <= 1.1, "verify decoded {ratio:.2}x");

    let doctored = with_index(&bytes, |index| {
        let e = index
            .entries
            .iter_mut()
            .find(|e| e.path == b"big.bin")
            .unwrap();
        e.hash.as_mut().unwrap()[31] ^= 0x80;
    });
    let a = Archive::open(doctored.into_boxed_slice(), limits).unwrap();
    let err = a.verify().unwrap_err().to_string();
    assert!(err.contains("big.bin"), "{err}");
}
