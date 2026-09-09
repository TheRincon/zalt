//! Compression Dictionary Transport, end to end against the reference.
//!
//! The unit tests in `src/dcz.rs` check the header parser against bytes this
//! crate wrote itself, which proves it agrees with itself. These build a real
//! `dcz` stream — the reference compresses against a real dictionary, we
//! prepend the RFC 9842 header — and check three separate claims:
//!
//! 1. our SHA-256 agrees with the system's, so the hash we compute is the hash
//!    an origin would put in the header;
//! 2. `zstd` itself decodes the full stream, header and all, which is the
//!    wire-compatibility claim the whole design rests on;
//! 3. zalt resolves the dictionary from the hash and decodes byte-exactly.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use zalt_recipe::dcz::{self, DictionaryStore, PRELUDE_LEN, PRELUDE_MAGIC};
use zalt_recipe::sha256::Sha256;

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

fn zstd_binary() -> Option<String> {
    let zstd = std::env::var("ZSTD").unwrap_or_else(|_| "zstd".into());
    Command::new(&zstd)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| zstd)
}

/// Text to use as dictionary and payload.
///
/// The RFC itself: present in the repo, generic, and nothing to do with any
/// content class. Split so the dictionary and the payload are different halves
/// of similar prose — which is the shape `dcz` is for.
fn corpus() -> Option<(Vec<u8>, Vec<u8>)> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../doc/rfc9842.txt");
    let text = std::fs::read(path).ok()?;
    let half = text.len() / 2;
    Some((text[..half].to_vec(), text[half..].to_vec()))
}

/// Our SHA-256 against the system's, on real data.
///
/// The NIST vectors prove the algorithm on four fixed inputs. This proves it on
/// 40 KB of arbitrary bytes against an independent implementation — the same
/// reasoning that put `ruzstd` in the codec's dev-dependencies as a second
/// oracle. A hash that disagrees with the rest of the world is not a hash.
#[test]
fn our_sha256_agrees_with_the_system_implementation() {
    let Some((dictionary, _)) = corpus() else {
        eprintln!("doc/rfc9842.txt not present; skipping");
        return;
    };

    // `shasum -a 256` on macOS, `sha256sum` on Linux. Whichever exists.
    let (program, args): (&str, &[&str]) = if Command::new("sha256sum")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        ("sha256sum", &[])
    } else {
        ("shasum", &["-a", "256"])
    };

    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => {
            eprintln!("no system sha256 tool; skipping");
            return;
        }
    };
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(&dictionary)
        .expect("writes");
    let out = child.wait_with_output().expect("runs");
    assert!(out.status.success(), "{program} failed");

    let theirs = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .expect("a digest")
        .to_string();
    let ours = String::from_utf8(Sha256::digest(&dictionary).to_hex().to_vec()).expect("ascii");

    assert_eq!(ours, theirs, "our SHA-256 disagrees with {program}");
}

/// Build a `dcz` stream and take it apart again, three ways.
#[test]
fn a_dcz_stream_round_trips_through_both_decoders() {
    let (Some(zstd), Some((dictionary, payload))) = (zstd_binary(), corpus()) else {
        eprintln!("zstd or corpus not present; skipping");
        return;
    };

    let dir = scratch("dcz-roundtrip");
    let dict_path = dir.join("raw.dict");
    let payload_path = dir.join("payload.bin");
    std::fs::write(&dict_path, &dictionary).unwrap();
    std::fs::write(&payload_path, &payload).unwrap();

    // RFC 9842 §5: "The dictionary used for the dcz content encoding is a raw
    // dictionary type". `zstd -D` on a file with no dictionary magic uses it
    // as raw content, which is exactly what a content prefix is.
    let compressed = Command::new(&zstd)
        .args(["-q", "-c", "-6", "-D"])
        .arg(&dict_path)
        .arg(&payload_path)
        .output()
        .expect("zstd runs");
    assert!(compressed.status.success(), "zstd failed to compress");

    // The header an origin would prepend.
    let hash = Sha256::digest(&dictionary);
    let mut stream = PRELUDE_MAGIC.to_vec();
    stream.extend_from_slice(hash.as_bytes());
    stream.extend_from_slice(&compressed.stdout);
    assert_eq!(&stream[..PRELUDE_LEN], &stream[..8 + 32]);

    // --- 1. the reference decodes the whole stream, header included.
    //
    // This is the claim the design rests on: the header is a legal zstd
    // skippable frame, so a decoder that has never heard of `dcz` steps over
    // it. If this ever fails, the format is not what §5 says it is.
    let stream_path = dir.join("resource.dcz");
    std::fs::write(&stream_path, &stream).unwrap();
    let reference = Command::new(&zstd)
        .args(["-q", "-d", "-c", "-D"])
        .arg(&dict_path)
        .arg(&stream_path)
        .output()
        .expect("zstd runs");
    assert!(
        reference.status.success(),
        "the reference refused a dcz stream: {}",
        String::from_utf8_lossy(&reference.stderr)
    );
    assert_eq!(reference.stdout, payload, "reference output differs");

    // --- 2. zalt skips the header the same way, with no dcz code involved.
    let mut whole =
        zalt_codec::Decoder::<zalt_codec::Scalar>::new(zalt_codec::params::DecodeLimits::default());
    whole
        .load_dictionary(zalt_codec::dict::Dictionary::parse(&dictionary).expect("raw dictionary"));
    let mut got = Vec::new();
    whole
        .decompress(&stream, &mut got)
        .expect("zalt must skip the dcz header like any skippable frame");
    assert_eq!(got, payload, "zalt output differs on the whole stream");

    // --- 3. the mechanism proper: resolve the dictionary from the hash.
    let mut store = DictionaryStore::new();
    let filed = store.insert(&dictionary).expect("raw dictionaries parse");
    assert_eq!(filed, hash, "the store filed it under the wire's identity");

    let (resolved, frame) = store
        .resolve(&stream)
        .expect("well-formed stream")
        .expect("the dictionary is held");
    assert_eq!(
        frame,
        &compressed.stdout[..],
        "the frame follows the header"
    );

    let mut decoder =
        zalt_codec::Decoder::<zalt_codec::Scalar>::new(zalt_codec::params::DecodeLimits::default());
    decoder.load_dictionary(resolved.clone());
    let mut out = Vec::new();
    decoder.decompress(frame, &mut out).expect("decodes");
    assert_eq!(out, payload, "resolved-dictionary output differs");
}

/// A stream naming a dictionary we hold, but decoded against a different one.
///
/// The failure `dcz` exists to prevent. A raw dictionary carries no
/// `Dictionary_ID`, so the codec cannot tell it has the wrong one — this is
/// the evidence for that claim rather than an assertion of it.
#[test]
fn the_wrong_dictionary_is_not_detected_by_the_codec_which_is_why_the_hash_exists() {
    let (Some(zstd), Some((dictionary, payload))) = (zstd_binary(), corpus()) else {
        eprintln!("zstd or corpus not present; skipping");
        return;
    };

    let dir = scratch("dcz-wrong-dict");
    let right = dir.join("right.dict");
    let wrong = dir.join("wrong.dict");
    let payload_path = dir.join("payload.bin");
    std::fs::write(&right, &dictionary).unwrap();
    // Same length, different bytes: a dictionary that is structurally fine and
    // simply not the one used to compress.
    let other: Vec<u8> = dictionary.iter().map(|b| b.wrapping_add(1)).collect();
    std::fs::write(&wrong, &other).unwrap();
    std::fs::write(&payload_path, &payload).unwrap();

    let compressed = Command::new(&zstd)
        .args(["-q", "-c", "-6", "-D"])
        .arg(&right)
        .arg(&payload_path)
        .output()
        .expect("zstd runs");
    assert!(compressed.status.success());

    let mut decoder =
        zalt_codec::Decoder::<zalt_codec::Scalar>::new(zalt_codec::params::DecodeLimits::default());
    decoder.load_dictionary(zalt_codec::dict::Dictionary::parse(&other).expect("raw dictionary"));
    let mut out = Vec::new();
    let result = decoder.decompress(&compressed.stdout, &mut out);

    // Either outcome is possible and neither is "this is the wrong
    // dictionary": the frame may fail its checksum, or it may decode to
    // something that is simply not the payload. What must NOT happen is
    // silently returning the right answer, which would mean the dictionary was
    // not being used at all and this test proves nothing.
    let undetected = result.is_ok() && out != payload;
    let detected_late = result.is_err();
    assert!(
        undetected || detected_late,
        "decoding with the wrong dictionary produced the correct payload, \
         so the dictionary is not affecting the decode and this test is vacuous"
    );

    // And the hash is what closes it, before a byte is decoded.
    let mut store = DictionaryStore::new();
    store.insert(&other).unwrap();
    let mut stream = PRELUDE_MAGIC.to_vec();
    stream.extend_from_slice(Sha256::digest(&dictionary).as_bytes());
    stream.extend_from_slice(&compressed.stdout);
    assert!(
        store.resolve(&stream).expect("well formed").is_none(),
        "a store holding only the wrong dictionary must not resolve this stream"
    );
}

#[test]
fn the_required_window_covers_the_dictionaries_we_ship() {
    // RFC 9842 §5 obliges a dcz client to handle at least 8 MB, or 1.25x the
    // dictionary where that is larger. Our content-prefix dictionaries are
    // tens of kilobytes, so the floor governs — worth pinning, because the
    // number that governs changes silently if a dictionary ever grows past
    // 6.4 MB and nothing else would notice.
    assert_eq!(dcz::required_window_log(47_000), 23);
    assert_eq!(
        dcz::required_window_log(47_000),
        zalt_embed_untrusted_window_log(),
        "the dcz floor and the untrusted default must not disagree silently"
    );
}

/// The window ceiling `zalt-embed` applies to untrusted input.
///
/// Spelled as a constant rather than a dependency: `zalt-recipe` deliberately
/// does not depend on L2, and adding that dependency to assert one number
/// would trade the layering for a test.
fn zalt_embed_untrusted_window_log() -> u8 {
    23
}
