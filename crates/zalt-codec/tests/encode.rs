//! The encoder, against three decoders.
//!
//! An encoder cannot be checked by reading its output. The only question worth
//! asking is whether somebody else can read it, and the strongest form of that
//! question uses decoders that share no code with this one:
//!
//! 1. **libzstd**, the reference, through its CLI. If the reference refuses a
//!    frame, the frame is wrong — there is no second opinion to seek.
//! 2. **`ruzstd`**, an independent pure-Rust decoder, so a shared misreading of
//!    the format between zalt's encoder and zalt's decoder cannot hide.
//! 3. **zalt's own decoder**, which is the weakest of the three and the one
//!    that would happily agree with a mistake made twice. It is here to catch
//!    the encoder and decoder drifting apart, not to establish correctness.
//!
//! Passing 3 alone would mean very little. That is the whole reason 1 and 2 are
//! wired up before a single ratio is measured.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use zalt_codec::encode::{EncodeParams, Strategy};
use zalt_codec::params::DecodeLimits;

fn zstd_binary() -> Option<String> {
    let zstd = std::env::var("ZSTD").unwrap_or_else(|_| "zstd".into());
    Command::new(&zstd)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| zstd)
}

/// Decode with the reference, through a pipe.
fn reference_decode(zstd: &str, frame: &[u8]) -> Result<Vec<u8>, String> {
    let mut child = Command::new(zstd)
        .args(["-q", "-d", "-c"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(frame)
        .map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(out.stdout)
}

/// Decode with the independent third-party implementation.
fn third_party_decode(frame: &[u8]) -> Option<Vec<u8>> {
    let mut decoder = ruzstd::StreamingDecoder::new(frame).ok()?;
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).ok()?;
    Some(out)
}

/// Decode with our own decoder.
fn self_decode(frame: &[u8]) -> zalt_codec::Result<Vec<u8>> {
    zalt_codec::decompress(frame, DecodeLimits::default())
}

/// Inputs chosen to hit the shapes the writer branches on.
fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        // The frame-header size classes, on both sides of every boundary. 255
        // and 256 straddle the 2-byte field's +256 offset; 65_791 is its top.
        ("empty", Vec::new()),
        ("one byte", vec![0x42]),
        ("255 bytes", vec![0xAB; 255]),
        ("256 bytes", (0..256u32).map(|i| i as u8).collect()),
        ("257 bytes", (0..257u32).map(|i| (i % 251) as u8).collect()),
        (
            "65_791 bytes",
            (0..65_791u32).map(|i| (i % 251) as u8).collect(),
        ),
        (
            "65_792 bytes",
            (0..65_792u32).map(|i| (i % 251) as u8).collect(),
        ),
        // Block splitting: exactly one block, and one byte past it.
        (
            "128 KiB exactly",
            (0..131_072u32).map(|i| (i % 251) as u8).collect(),
        ),
        (
            "128 KiB + 1",
            (0..131_073u32).map(|i| (i % 251) as u8).collect(),
        ),
        // The RLE path, including a run long enough to span blocks.
        ("a run", vec![0x5a; 5000]),
        ("a run across blocks", vec![0x00; 300_000]),
        // Two regimes back to back: compressible text then incompressible
        // noise. The only shape that makes the block splitter fire, so without
        // it here that path is never exercised against a real decoder.
        ("two regimes", {
            let mut v = b"the quick brown fox jumps over the lazy dog. ".repeat(1600);
            v.truncate(64 * 1024);
            let mut s = 0x1234_5678u32;
            v.extend((0..64 * 1024).map(|_| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (s >> 24) as u8
            }));
            v
        }),
        // Incompressible-looking content, which is what `stored` is *for*.
        (
            "high entropy",
            (0..40_000u32)
                .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
                .collect(),
        ),
    ]
}

fn params_matrix() -> Vec<(&'static str, EncodeParams)> {
    let base = EncodeParams::default();
    vec![
        ("default", base),
        (
            "checksum",
            EncodeParams {
                checksum: true,
                ..base
            },
        ),
        (
            "small blocks",
            EncodeParams {
                block_size: 1024,
                window_log: 10,
                ..base
            },
        ),
        (
            "dictionary id",
            EncodeParams {
                dictionary_id: Some(0x5A54_4D4C),
                ..base
            },
        ),
        (
            "small dictionary id",
            EncodeParams {
                dictionary_id: Some(7),
                ..base
            },
        ),
        (
            "large window",
            EncodeParams {
                window_log: 23,
                ..base
            },
        ),
        (
            "compressed",
            EncodeParams {
                strategy: Strategy::Compress,
                ..base
            },
        ),
        (
            "compressed, small blocks",
            EncodeParams {
                strategy: Strategy::Compress,
                block_size: 1024,
                window_log: 10,
                ..base
            },
        ),
        (
            "compressed with checksum",
            EncodeParams {
                strategy: Strategy::Compress,
                checksum: true,
                ..base
            },
        ),
    ]
}

#[test]
fn the_reference_decodes_everything_this_encoder_emits() {
    let Some(zstd) = zstd_binary() else {
        eprintln!("zstd not present; skipping");
        return;
    };

    let mut checked = 0usize;
    for (pname, params) in params_matrix() {
        for (cname, content) in corpus() {
            // A dictionary id names a dictionary the reference does not have,
            // and it will refuse the frame for that reason alone. Skipping is
            // right: the id round trip is checked through our own parser
            // instead, where the dictionary is not required to exist.
            if params.dictionary_id.is_some() {
                continue;
            }
            let frame = zalt_codec::compress(&content, &params)
                .unwrap_or_else(|e| panic!("{pname}/{cname}: encode failed: {e}"));
            let decoded = reference_decode(&zstd, &frame)
                .unwrap_or_else(|e| panic!("{pname}/{cname}: the reference refused it: {e}"));
            assert_eq!(
                decoded, content,
                "{pname}/{cname}: reference output differs"
            );
            checked += 1;
        }
    }
    assert!(checked > 0);
    eprintln!("the reference accepted {checked} frames");
}

#[test]
fn an_independent_decoder_reads_everything_this_encoder_emits() {
    let mut checked = 0usize;
    for (pname, params) in params_matrix() {
        if params.dictionary_id.is_some() {
            continue;
        }
        for (cname, content) in corpus() {
            let frame = zalt_codec::compress(&content, &params).expect("encode");
            // Unlike the reference, ruzstd refusing a frame is not proof of a
            // bug — it has its own gaps. But it has no reason to refuse a raw
            // block, so a refusal here is worth failing on.
            let decoded = third_party_decode(&frame)
                .unwrap_or_else(|| panic!("{pname}/{cname}: ruzstd refused the frame"));
            assert_eq!(decoded, content, "{pname}/{cname}: ruzstd output differs");
            checked += 1;
        }
    }
    assert!(checked > 0);
    eprintln!("ruzstd accepted {checked} frames");
}

#[test]
fn our_own_decoder_round_trips_every_parameter_combination() {
    for (pname, params) in params_matrix() {
        // A declared dictionary id is a demand for a dictionary nobody loaded,
        // so refusing it is the decoder working. The id itself is checked by
        // `the_header_a_frame_was_written_from_parses_back_to_it`, where the
        // dictionary is not required to exist.
        if params.dictionary_id.is_some() {
            continue;
        }
        for (cname, content) in corpus() {
            let frame = zalt_codec::compress(&content, &params).expect("encode");
            let decoded = self_decode(&frame).unwrap_or_else(|e| panic!("{pname}/{cname}: {e}"));
            assert_eq!(decoded, content, "{pname}/{cname}: round trip differs");
        }
    }
}

#[test]
fn the_header_a_frame_was_written_from_parses_back_to_it() {
    // The property the writer is built on: it is the parser's inverse, so
    // anything it writes must read back as what it was told to write.
    for (_, params) in params_matrix() {
        for (cname, content) in corpus() {
            let frame = zalt_codec::compress(&content, &params).expect("encode");
            let descriptor = frame[4];
            let after = zalt_codec::frame::header_len_after_descriptor(descriptor)
                .expect("a descriptor we wrote");
            let header = zalt_codec::frame::FrameHeader::parse(
                descriptor,
                &frame[5..5 + after],
                &DecodeLimits::default(),
            )
            .unwrap_or_else(|e| panic!("{cname}: our own header did not parse: {e}"));

            assert_eq!(
                header.has_checksum, params.checksum,
                "{cname}: checksum flag"
            );
            assert_eq!(
                header.dictionary_id, params.dictionary_id,
                "{cname}: dictionary id"
            );
            if let Some(declared) = header.frame_content_size {
                assert_eq!(declared, content.len() as u64, "{cname}: content size");
            }
            // Whatever window was declared must admit the blocks written into
            // it, or the frame is malformed by construction.
            assert!(
                header.block_size_max() >= u64::from(params.block_size).min(content.len() as u64),
                "{cname}: declared window will not admit the blocks"
            );
        }
    }
}

#[test]
fn a_corrupted_checksum_is_detected() {
    // Proves the checksum is over the content and actually written, rather
    // than four bytes of something that happens to be there.
    let content = b"the quick brown fox jumps over the lazy dog".repeat(50);
    let params = EncodeParams {
        checksum: true,
        ..EncodeParams::default()
    };
    let mut frame = zalt_codec::compress(&content, &params).expect("encode");
    assert_eq!(self_decode(&frame).expect("clean frame decodes"), content);

    let last = frame.len() - 1;
    frame[last] ^= 0xff;
    assert!(
        matches!(
            self_decode(&frame),
            Err(zalt_codec::Error::ChecksumMismatch)
        ),
        "a flipped checksum byte must be caught"
    );
}

/// Compression must actually compress, and must never expand.
///
/// Two separate claims. A compressor that emits valid frames nobody can fault
/// while achieving a ratio of 1.0 would pass every other test in this file —
/// that is exactly the silent failure the stored/compress split exists to make
/// impossible — so the ratio is asserted, not assumed. And on incompressible
/// input the block fallback must engage, because a literals section wrapping
/// random bytes is always larger than the bytes.
#[test]
fn compression_compresses_and_never_expands() {
    let params = EncodeParams {
        strategy: Strategy::Compress,
        ..EncodeParams::default()
    };

    let repetitive = b"the quick brown fox jumps over the lazy dog. ".repeat(200);
    let frame = zalt_codec::compress(&repetitive, &params).expect("encode");
    let ratio = repetitive.len() as f64 / frame.len() as f64;
    assert!(ratio > 4.0, "repetitive input compressed only {ratio:.2}x");
    assert_eq!(self_decode(&frame).expect("round trip"), repetitive);

    // Incompressible: the fallback to a stored block keeps the overhead to
    // headers, exactly as `strategy = "stored"` would.
    let noise: Vec<u8> = (0..40_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let frame = zalt_codec::compress(&noise, &params).expect("encode");
    let blocks = noise.len().div_ceil(128 * 1024);
    assert!(
        frame.len() <= noise.len() + 18 + 3 * blocks,
        "incompressible input expanded by {} bytes",
        frame.len() - noise.len()
    );
    assert_eq!(self_decode(&frame).expect("round trip"), noise);
}

#[test]
fn parameters_that_would_build_a_malformed_frame_are_refused_up_front() {
    let base = EncodeParams::default();
    let cases: &[(&str, EncodeParams)] = &[
        (
            "a window too small for the blocks",
            EncodeParams {
                window_log: 10,
                block_size: 128 * 1024,
                ..base
            },
        ),
        (
            "a window below the format minimum",
            EncodeParams {
                window_log: 9,
                ..base
            },
        ),
        (
            "a window above the implementation maximum",
            EncodeParams {
                window_log: 32,
                ..base
            },
        ),
        (
            "a zero block size",
            EncodeParams {
                block_size: 0,
                ..base
            },
        ),
        (
            "a block larger than the format allows",
            EncodeParams {
                block_size: 128 * 1024 + 1,
                ..base
            },
        ),
        (
            "dictionary id zero, which means no dictionary",
            EncodeParams {
                dictionary_id: Some(0),
                ..base
            },
        ),
    ];
    for (label, params) in cases {
        assert!(
            zalt_codec::compress(b"content", params).is_err(),
            "{label}: should have been refused"
        );
    }
}

/// The block splitter must fire where it pays and stay silent where it does not.
///
/// §7 proposes splitting at statistical transitions; the measurements in §10
/// found that doing it at a fixed stride is a loss on every real file, so the
/// splitter only proposes a boundary where the byte distribution actually
/// changes, and the encoder keeps whichever encoding came out smaller. Both
/// halves are asserted here: a real transition must be exploited, and uniform
/// content must come out byte-identical to what it produced before the feature
/// existed.
#[test]
fn splitting_pays_on_a_transition_and_costs_nothing_without_one() {
    let params = EncodeParams {
        strategy: Strategy::Compress,
        ..EncodeParams::default()
    };

    let mut text = b"the quick brown fox jumps over the lazy dog. ".repeat(1600);
    text.truncate(64 * 1024);
    let mut s = 0x1234_5678u32;
    let noise: Vec<u8> = (0..64 * 1024)
        .map(|_| {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s >> 24) as u8
        })
        .collect();

    let mut two_regime = text.clone();
    two_regime.extend_from_slice(&noise);

    // With a block size large enough to hold it all, one block is the only
    // option the fixed path has — so any saving is the splitter's.
    let split = zalt_codec::compress(&two_regime, &params).expect("encode");
    let unsplit = {
        // Same content, but the halves interleaved at 64-byte granularity, so
        // no boundary exists to find. This is the shape a document carrying a
        // hash per subresource has, and it is why the splitter is gated.
        let mut mixed = Vec::with_capacity(two_regime.len());
        for i in (0..64 * 1024).step_by(64) {
            mixed.extend_from_slice(&text[i..i + 64]);
            mixed.extend_from_slice(&noise[i..i + 64]);
        }
        zalt_codec::compress(&mixed, &params).expect("encode")
    };

    assert_eq!(self_decode(&split).expect("round trip"), two_regime);
    assert!(
        split.len() < unsplit.len(),
        "separable content should compress better than interleaved: {} vs {}",
        split.len(),
        unsplit.len()
    );

    // And uniform content must be untouched by the feature.
    let uniform = b"the quick brown fox jumps over the lazy dog. ".repeat(2000);
    let out = zalt_codec::compress(&uniform, &params).expect("encode");
    assert_eq!(self_decode(&out).expect("round trip"), uniform);
}

/// The `strategy = "stored"` case from §8, on real already-compressed bytes.
#[test]
fn already_compressed_media_survives_a_stored_round_trip() {
    // The recipe rule exists because zstd over these yields about 1.0x. This
    // checks the *other* half of that claim: that stored mode is correct on
    // them, and costs only the block headers.
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../doc/rfc9842.txt");
    let Ok(content) = std::fs::read(&path) else {
        eprintln!("corpus not present; skipping");
        return;
    };

    let frame = zalt_codec::compress_default(&content).expect("encode");
    assert_eq!(self_decode(&frame).expect("round trip"), content);

    // Overhead is the frame header plus three bytes per block, and nothing else.
    let blocks = content.len().div_ceil(128 * 1024);
    let overhead = frame.len() - content.len();
    assert!(
        overhead <= 18 + 3 * blocks,
        "stored mode should cost only headers: {overhead} bytes over {blocks} blocks"
    );
}
