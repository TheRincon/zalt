#![allow(dead_code)]

use std::collections::HashMap;

use zlta_seekable_probe::{
    archive::{self, Archive, SeekProfile, WriterOptions, MAGIC_INDEX},
    corpus::{Corpus, Rng},
    encoder::{FrameEncoder, ReferenceEncoder},
    index,
    seekable::RefSeekable,
};

/// The one place to plug in your own encoder.
///
/// Replace the body with e.g. `Box::new(my_zstd::ProbeEncoder::new(level))`, where
/// `ProbeEncoder` implements `zlta_seekable_probe::encoder::FrameEncoder`. Every
/// test then writes its archives with your encoder and reads them back with the
/// *reference* decoder and seekable reader.
pub fn encoder_at(level: i32) -> Box<dyn FrameEncoder> {
    if std::env::var("ZLTA_ENCODER").as_deref() == Ok("reference") {
        return Box::new(ReferenceEncoder::new(level));
    }
    Box::new(ZaltEncoder { level })
}

/// zalt's encoder, as §5.2 asks of a data frame: a content checksum, at the
/// level the test requested. Since the probe moved into the zalt workspace this
/// is the default — the probe is the oracle *for zalt* — and
/// `ZLTA_ENCODER=reference` puts libzstd back, which is how a failure here is
/// told apart from a failure of the probe. zalt has no negative levels; they
/// clamp to 1, which `describe` says so a failure is not misattributed.
pub struct ZaltEncoder {
    level: i32,
}

impl FrameEncoder for ZaltEncoder {
    fn encode_frame(&mut self, src: &[u8]) -> Result<Vec<u8>, String> {
        let params = zalt_codec::encode::EncodeParams {
            strategy: zalt_codec::encode::Strategy::Compress,
            level: self.level,
            checksum: true,
            ..Default::default()
        };
        zalt_codec::encode::compress(src, &params).map_err(|e| e.to_string())
    }

    fn describe(&self) -> String {
        format!(
            "zalt encoder, level {} (effective {})",
            self.level,
            zalt_codec::encode::level::effective(self.level)
        )
    }
}

/// Compression level for most tests: `ZLTA_LEVEL`, default 3.
pub fn level() -> i32 {
    std::env::var("ZLTA_LEVEL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3)
}

pub fn encoder() -> Box<dyn FrameEncoder> {
    encoder_at(level())
}

/// Small frames, so a 5 MiB corpus yields dozens of frames and many boundaries.
pub fn small_frames() -> WriterOptions {
    WriterOptions {
        solid_target: 64 << 10,
        chunk: 256 << 10,
        ..Default::default()
    }
}

pub fn build(c: &Corpus, o: &WriterOptions) -> Archive {
    archive::write(&c.body, &c.entries, encoder().as_mut(), o)
        .unwrap_or_else(|e| panic!("write failed: {e}"))
}

pub fn describe(a: &Archive) -> String {
    let zero = a.listed.iter().filter(|&&i| a.frames[i].dsize == 0).count();
    format!(
        "{} bytes; {} seek-table entries ({} zero-size); profile {:?}; seek checksums {}",
        a.bytes.len(),
        a.listed.len(),
        zero,
        a.options.profile,
        a.options.seek_checksums
    )
}

pub fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}

pub fn first_mismatch(a: &[u8], b: &[u8]) -> usize {
    a.iter()
        .zip(b)
        .position(|(x, y)| x != y)
        .unwrap_or(a.len().min(b.len()))
}

#[track_caller]
fn assert_bytes(got: &[u8], want: &[u8], what: &str) {
    assert!(
        got == want,
        "{what}: got {} bytes, want {}, first difference at {}",
        got.len(),
        want.len(),
        first_mismatch(got, want)
    );
}

/// Everything Q1 asks, checked against zstd's reference seekable reader, for an
/// archive written with the spec's §8.2 profile.
pub fn verify_reference_reader(a: &Archive, c: &Corpus) {
    assert_eq!(a.options.profile, SeekProfile::AllFrames);
    let mut zs =
        RefSeekable::open(&a.bytes).expect("reference reader rejected the ZLTA seek table");
    let n = zs.num_frames();
    let stream_len = a.tar_stream.len() as u64;
    assert_eq!(n as usize, a.listed.len(), "number of seek-table entries");

    // 1. Offsets: the reader derives file and stream offsets from the table alone.
    for (i, &fi) in a.listed.iter().enumerate() {
        let (i, f) = (i as u32, &a.frames[fi]);
        assert_eq!(
            zs.frame_c_offset(i),
            f.file_offset,
            "file offset of entry {i} ({:?})",
            f.role
        );
        assert_eq!(
            zs.frame_c_size(i),
            f.csize,
            "on-disk size of entry {i} ({:?})",
            f.role
        );
        assert_eq!(
            zs.frame_d_offset(i),
            f.stream_offset,
            "stream offset of entry {i} ({:?})",
            f.role
        );
        assert_eq!(
            zs.frame_d_size(i),
            f.dsize,
            "content size of entry {i} ({:?})",
            f.role
        );
    }
    assert_eq!(zs.stream_len(), stream_len, "tar stream length");
    assert_eq!(
        zs.frame_c_offset(n - 1) + zs.frame_c_size(n - 1),
        a.seek_table_offset(),
        "listed frames end exactly where the seek table starts"
    );

    // 2. Offset lookup never lands on a zero-size entry.
    for (i, &fi) in a.listed.iter().enumerate() {
        let f = &a.frames[fi];
        if f.dsize > 0 {
            assert_eq!(
                zs.offset_to_frame(f.stream_offset),
                i as u32,
                "first byte of entry {i}"
            );
            assert_eq!(
                zs.offset_to_frame(f.stream_offset + f.dsize - 1),
                i as u32,
                "last byte of entry {i}"
            );
        } else if f.stream_offset < stream_len {
            let t = zs.offset_to_frame(f.stream_offset);
            let chosen = &a.frames[a.listed[t as usize]];
            assert!(
                chosen.dsize > 0 && chosen.stream_offset == f.stream_offset,
                "stream offset {} of zero-size entry {i} ({:?}) resolved to entry {t} ({:?})",
                f.stream_offset,
                f.role,
                chosen.role
            );
        }
    }
    assert_eq!(
        zs.offset_to_frame(stream_len),
        n,
        "end of stream maps past the last entry"
    );

    // 3. The whole tar stream in one call.
    assert_bytes(
        &zs.read_at(0, stream_len as usize).unwrap(),
        &a.tar_stream,
        "full read",
    );

    // 4. Every seek-table entry through decompressFrame, zero-size ones included.
    for (i, &fi) in a.listed.iter().enumerate() {
        let f = &a.frames[fi];
        let got = zs
            .decompress_frame(i as u32)
            .unwrap_or_else(|e| panic!("entry {i} ({:?}): {e}", f.role));
        let s = f.stream_offset as usize;
        assert_bytes(
            &got,
            &a.tar_stream[s..s + f.dsize as usize],
            &format!("decompressFrame({i})"),
        );
    }

    // 5. Short reads straddling every data-frame boundary (where zero-size entries sit).
    for f in a
        .frames
        .iter()
        .filter(|f| f.dsize > 0 && f.stream_offset > 0)
    {
        let off = f.stream_offset - 7;
        let got = zs.read_at(off, 14).unwrap();
        assert_bytes(
            &got,
            &a.tar_stream[off as usize..off as usize + 14],
            "boundary read",
        );
    }

    // 6. Random ranges in random order, exercising the reader's frame reuse and resets.
    let mut rng = Rng::new(0x5EED);
    for _ in 0..400 {
        let off = rng.below(stream_len);
        let len = 1 + rng.below((stream_len - off).min(1 << 20)) as usize;
        let got = zs.read_at(off, len).unwrap();
        assert_bytes(
            &got,
            &a.tar_stream[off as usize..off as usize + len],
            &format!("read_at({off}, {len})"),
        );
    }

    // 7. §11.1 indexed open driven by the reference reader: the last entry is the
    //    Central Index; its records locate every member in the tar stream.
    let last = n - 1;
    let (off, size) = (
        zs.frame_c_offset(last) as usize,
        zs.frame_c_size(last) as usize,
    );
    let frame = &a.bytes[off..off + size];
    assert_eq!(
        le32(frame),
        MAGIC_INDEX,
        "last seek-table entry is the Central Index"
    );
    let idx = index::decode(&frame[8..], 64 << 20).expect("Central Index decodes");
    assert_eq!(idx.tar_stream_size, stream_len);
    assert_eq!(idx.entries.len(), c.entries.len());
    let contents: HashMap<&str, &[u8]> = c
        .files
        .iter()
        .map(|(p, d)| (p.as_str(), d.as_slice()))
        .collect();
    for (ie, te) in idx.entries.iter().zip(&c.entries) {
        assert_eq!(
            (
                &ie.path,
                ie.header_offset,
                ie.data_offset(),
                ie.size,
                &ie.link
            ),
            (
                &te.path,
                te.header_offset,
                te.data_offset,
                te.size,
                &te.link
            )
        );
        let hdr = zs.read_at(ie.header_offset, 512).unwrap();
        assert_eq!(
            &hdr[257..263],
            b"ustar\0",
            "tar header at Header_Offset of {}",
            ie.path
        );
        if ie.typeflag == b'0' {
            let got = zs.read_at(ie.data_offset(), ie.size as usize).unwrap();
            assert_bytes(&got, contents[ie.path.as_str()], &ie.path);
        }
    }
}
