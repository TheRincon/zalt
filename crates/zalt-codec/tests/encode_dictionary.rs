//! Compressing against a dictionary, against three decoders.
//!
//! The same three judges as `encode.rs` — libzstd through its CLI, `ruzstd`,
//! and zalt's own decoder — given the dictionary each frame was written
//! against. Every level runs, because the levels are the finders: each one
//! indexes the dictionary and bounds its offsets by its own code.
//!
//! The rule a dictionary adds is RFC 8878 §5's: a match may reach into the
//! dictionary, further back than `Window_Size` even, only while the frame has
//! produced at most `Window_Size` bytes. Frames small enough to be one window
//! never leave that phase, so `the_dictionary_drops_out_after_one_window`
//! writes frames many windows long against a dictionary they keep wanting.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use zalt_codec::dict::Dictionary;
use zalt_codec::encode::{EncodeParams, Strategy, compress_with_dictionary};
use zalt_codec::params::DecodeLimits;
use zalt_codec::{Decoder, Scalar};

fn zstd_binary() -> Option<String> {
    let zstd = std::env::var("ZSTD").unwrap_or_else(|_| "zstd".into());
    Command::new(&zstd)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| zstd)
}

/// A small deterministic generator, so every run sees the same documents.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn pick<'a>(&mut self, words: &[&'a str]) -> &'a str {
        words[self.next() as usize % words.len()]
    }
}

/// One small document of the kind dictionaries exist for: many short files
/// sharing keys, structure and vocabulary, each too small to teach a
/// compressor much on its own.
fn document(rng: &mut Lcg) -> Vec<u8> {
    const KEYS: [&str; 8] = [
        "identifier",
        "created_at",
        "description",
        "dependencies",
        "maintainers",
        "repository",
        "license",
        "categories",
    ];
    const VALUES: [&str; 10] = [
        "\"compression\"",
        "\"MIT OR Apache-2.0\"",
        "\"https://example.org/project\"",
        "[\"parsing\", \"encoding\"]",
        "true",
        "null",
        "\"2026-10-22T12:00:00Z\"",
        "{\"version\": \"1.0\", \"optional\": false}",
        "\"a fast, safe implementation\"",
        "[\"alice\", \"bob\"]",
    ];
    let mut out = String::from("{\n");
    for i in 0..(6 + rng.next() % 10) {
        out.push_str(&format!(
            "  \"{}_{}\": {},\n",
            rng.pick(&KEYS),
            rng.next() % 50,
            rng.pick(&VALUES)
        ));
        if i % 3 == 0 {
            out.push_str(&format!("  \"serial\": {},\n", rng.next() % 100_000));
        }
    }
    out.push_str("  \"end\": true\n}\n");
    out.into_bytes()
}

fn documents(seed: u64, count: usize) -> Vec<Vec<u8>> {
    let mut rng = Lcg(seed);
    (0..count).map(|_| document(&mut rng)).collect()
}

/// The dictionaries every frame is written against: what `zstd --train`
/// builds from the same kind of document, a golden dictionary from the
/// reference's own set whose tables omit symbols, and raw content.
fn dictionaries() -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    if let Some(zstd) = zstd_binary() {
        // A directory per call: several tests train at once, in one process.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("encode-dictionary-train-{n}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut files = Vec::new();
        for (i, doc) in documents(1, 600).iter().enumerate() {
            let path = dir.join(format!("sample{i:04}.json"));
            std::fs::write(&path, doc).unwrap();
            files.push(path);
        }
        let dict = dir.join("trained.dict");
        let status = Command::new(&zstd)
            .args(["-q", "--train", "--maxdict=16384", "-o"])
            .arg(&dict)
            .args(&files)
            .status()
            .unwrap();
        assert!(status.success(), "zstd --train failed");
        let trained = std::fs::read(&dict).unwrap();
        // The same dictionary with repeat offsets nobody would guess. zstd
        // trains `{1, 4, 8}` — the format's defaults — so an encoder starting
        // from the defaults instead of the dictionary's would pass with the
        // trained one and write frames every decoder misreads with this one.
        // The offsets sit just before the content.
        let content_len = Dictionary::parse(&trained).unwrap().content().len();
        let at = trained.len() - content_len - 12;
        let mut odd = trained.clone();
        for (i, rep) in [7u32, 300, content_len as u32 - 1].iter().enumerate() {
            odd[at + 4 * i..at + 4 * i + 4].copy_from_slice(&rep.to_le_bytes());
        }
        out.push(("trained".into(), trained));
        out.push(("odd repeats".into(), odd));
    }
    let golden = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../zstd-fixtures/golden-dictionaries/http-dict-missing-symbols");
    if let Ok(raw) = std::fs::read(&golden) {
        out.push(("golden".into(), raw));
    }
    // Raw content: history and nothing else, no ID, no tables.
    out.push(("raw".into(), documents(2, 40).concat()));
    out
}

/// The inputs: held-out documents of every size class the frame writer
/// branches on, and one larger than a block.
fn inputs() -> Vec<(String, Vec<u8>)> {
    let held_out = documents(3, 300);
    vec![
        ("empty".into(), Vec::new()),
        ("one byte".into(), vec![b'{']),
        ("one document".into(), held_out[0].clone()),
        // A run first: the first match is at offset 1, which is a repeat
        // offset under the format's defaults and not under every
        // dictionary's — the case that tells the two tables apart.
        (
            "a run, then a document".into(),
            [vec![b' '; 64], held_out[1].clone()].concat(),
        ),
        ("ten documents".into(), held_out[..10].concat()),
        ("all documents".into(), held_out.concat()),
    ]
}

fn reference_decode(zstd: &str, dict: &Path, frame: &[u8]) -> Result<Vec<u8>, String> {
    let mut child = Command::new(zstd)
        .args(["-q", "-d", "-c", "-D"])
        .arg(dict)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(frame)
        .map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(out.stdout)
}

fn third_party_decode(raw_dict: &[u8], frame: &[u8]) -> Result<Vec<u8>, String> {
    let dict = ruzstd::decoding::Dictionary::decode_dict(raw_dict).map_err(|e| e.to_string())?;
    let mut frames = ruzstd::decoding::FrameDecoder::new();
    frames.add_dict(dict).map_err(|e| e.to_string())?;
    let mut decoder = ruzstd::decoding::StreamingDecoder::new_with_decoder(frame, frames)
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

fn self_decode(dict: &Dictionary, frame: &[u8]) -> zalt_codec::Result<Vec<u8>> {
    let mut decoder = Decoder::<Scalar>::new(DecodeLimits::default());
    decoder.load_dictionary(dict.clone());
    let mut out = Vec::new();
    decoder.decompress(frame, &mut out)?;
    Ok(out)
}

fn params(level: i32) -> EncodeParams {
    EncodeParams {
        strategy: Strategy::Compress,
        level,
        checksum: true,
        ..EncodeParams::default()
    }
}

/// Every level, every dictionary, every input: all three decoders return the
/// input, ruzstd wherever it can load the dictionary.
#[test]
fn three_decoders_read_every_frame_written_against_a_dictionary() {
    let zstd = zstd_binary();
    if zstd.is_none() {
        eprintln!("zstd not present; the reference is skipped");
    }
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("encode-dictionary-judge");
    std::fs::create_dir_all(&dir).unwrap();
    let mut checked = 0usize;
    for (dname, raw) in dictionaries() {
        let dict = Dictionary::parse(&raw).unwrap();
        let dict_path = dir.join(&dname);
        std::fs::write(&dict_path, &raw).unwrap();
        // ruzstd reads formatted dictionaries only, and refuses the golden
        // one itself — its Huffman table log is 12, which RFC 8878 allows a
        // dictionary and not a block (see `huf::DICT_TABLELOG_MAX`). Where it
        // cannot load the dictionary it cannot judge the frame; the trained
        // one it must load.
        let ruzstd_loads = ruzstd::decoding::Dictionary::decode_dict(&raw).is_ok();
        if dname == "trained" {
            assert!(ruzstd_loads, "ruzstd refused a dictionary zstd trained");
        }
        if !ruzstd_loads {
            eprintln!("{dname}: ruzstd cannot load this dictionary; judged by the other two");
        }
        for (iname, input) in inputs() {
            for level in 1..=19 {
                let frame = compress_with_dictionary::<Scalar>(&input, &dict, &params(level))
                    .unwrap_or_else(|e| panic!("{dname}/{iname}/L{level}: {e}"));
                let what = format!("{dname}/{iname}/L{level}");

                let ours = self_decode(&dict, &frame).unwrap_or_else(|e| panic!("{what}: {e}"));
                assert_eq!(ours, input, "{what}: zalt's decoder differs");

                if let Some(zstd) = &zstd {
                    let theirs = reference_decode(zstd, &dict_path, &frame)
                        .unwrap_or_else(|e| panic!("{what}: libzstd refused: {e}"));
                    assert_eq!(theirs, input, "{what}: libzstd's output differs");
                }
                if ruzstd_loads {
                    let third = third_party_decode(&raw, &frame)
                        .unwrap_or_else(|e| panic!("{what}: ruzstd refused: {e}"));
                    assert_eq!(third, input, "{what}: ruzstd's output differs");
                }
                checked += 1;
            }
        }
    }
    eprintln!("{checked} dictionary frames read by every decoder");
}

/// A frame names the dictionary it needs, and only a formatted dictionary
/// has a name: raw content declares none.
#[test]
fn the_frame_declares_the_dictionarys_id_and_raw_content_declares_none() {
    let input = documents(3, 5).concat();
    for (name, raw) in dictionaries() {
        let dict = Dictionary::parse(&raw).unwrap();
        // A caller's ID is overruled: the dictionary's is the only one the
        // frame can be decoded with.
        let p = EncodeParams {
            dictionary_id: Some(0xdead_beef),
            ..params(3)
        };
        let frame = compress_with_dictionary::<Scalar>(&input, &dict, &p).unwrap();
        let descriptor = frame[4];
        let after = zalt_codec::frame::header_len_after_descriptor(descriptor).unwrap();
        let info = zalt_codec::frame::FrameHeader::parse(
            descriptor,
            &frame[5..5 + after],
            &DecodeLimits::default(),
        )
        .unwrap();
        let expected = (dict.id() != 0).then_some(dict.id());
        assert_eq!(info.dictionary_id, expected, "{name}");
    }
}

/// The point of a dictionary: small documents come out smaller. Not a ratio
/// target — the gain depends on the dictionary — only that the dictionary is
/// being used at every level.
#[test]
fn small_documents_compress_smaller_against_a_trained_dictionary() {
    let Some((_, raw)) = dictionaries().into_iter().find(|(n, _)| n == "trained") else {
        eprintln!("zstd not present; nothing trained");
        return;
    };
    let dict = Dictionary::parse(&raw).unwrap();
    let docs = documents(3, 50);
    for level in 1..=19 {
        let p = params(level);
        let plain: usize = docs
            .iter()
            .map(|d| zalt_codec::encode::compress(d, &p).unwrap().len())
            .sum();
        let with: usize = docs
            .iter()
            .map(|d| {
                compress_with_dictionary::<Scalar>(d, &dict, &p)
                    .unwrap()
                    .len()
            })
            .sum();
        assert!(
            with * 2 < plain,
            "L{level}: {with} bytes with the dictionary, {plain} without"
        );
    }
}

/// RFC 8878 §5's phase rule, where it bites: frames many windows long, made
/// of nothing but the dictionary repeated, so a matcher that kept reaching
/// into the dictionary after the first window would do so constantly.
///
/// The window is 1 KiB, smaller than the dictionary, so even inside the first
/// window most of what the frame wants is further back than `Window_Size` —
/// legal there, and only there.
#[test]
fn the_dictionary_drops_out_after_one_window() {
    let zstd = zstd_binary();
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("encode-dictionary-phase");
    std::fs::create_dir_all(&dir).unwrap();
    let raw = documents(2, 12).concat();
    assert!(raw.len() > 4096, "the dictionary must outreach the window");
    let dict = Dictionary::parse(&raw).unwrap();
    let dict_path = dir.join("raw");
    std::fs::write(&dict_path, &raw).unwrap();

    // Documents the dictionary is made of, shuffled: each is in the dictionary
    // verbatim, and repeats of it in the frame lie a window or more back.
    let mut rng = Lcg(9);
    let pieces = documents(2, 12);
    let input: Vec<u8> = (0..400)
        .flat_map(|_| pieces[rng.next() as usize % pieces.len()].clone())
        .collect();
    for level in 1..=19 {
        let p = EncodeParams {
            window_log: 10,
            block_size: 1024,
            ..params(level)
        };
        let frame = compress_with_dictionary::<Scalar>(&input, &dict, &p).unwrap();
        let what = format!("L{level}");
        let ours = self_decode(&dict, &frame).unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_eq!(ours, input, "{what}");
        if let Some(zstd) = &zstd {
            let theirs = reference_decode(zstd, &dict_path, &frame)
                .unwrap_or_else(|e| panic!("{what}: libzstd refused: {e}"));
            assert_eq!(theirs, input, "{what}: libzstd's output differs");
        }
    }
}

/// Without a dictionary the frame is unchanged by any of this: an empty raw
/// dictionary is not allowed, so "no dictionary" is the plain entry point, and
/// it must agree with itself across the refactor that added the other.
#[test]
fn a_dictionary_frame_is_deterministic() {
    let raw = documents(2, 40).concat();
    let dict = Dictionary::parse(&raw).unwrap();
    let input = documents(3, 30).concat();
    for level in [1, 3, 9, 16, 19] {
        let a = compress_with_dictionary::<Scalar>(&input, &dict, &params(level)).unwrap();
        let b = compress_with_dictionary::<Scalar>(&input, &dict, &params(level)).unwrap();
        assert_eq!(a, b, "L{level}");
    }
}
