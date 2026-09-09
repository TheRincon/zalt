//! Compression Dictionary Transport — the `dcz` content encoding (RFC 9842 §5).
//!
//! A `dcz` stream is a 40-byte header followed by an ordinary zstd frame:
//!
//! ```text
//! 5e 2a 4d 18   magic, little-endian 0x184D2A5E
//! 20 00 00 00   length, little-endian 32
//! <32 bytes>    SHA-256 of the dictionary
//! <zstd frame>
//! ```
//!
//! The elegance is that those first eight bytes are a legal zstd **skippable
//! frame**, so a decoder that has never heard of `dcz` steps over the header
//! and decodes the frame — which then fails, correctly, because it needs a
//! dictionary. Nothing here changes how `zalt-codec` decodes anything; this
//! module only reads the header so a host knows *which* dictionary to load.
//!
//! ## Why this is L3 and not L1
//!
//! Layering permits either — `dcz` is a generic transport format, not a content
//! class, so `zalt-codec` naming it would break no rule, and `magic.rs` already
//! carries the constant. The deciding argument is cohesion: the entire purpose
//! of this header is *which dictionary to use*, which is the first row of §7's
//! table and the defining job of L3. `zalt-codec` stays RFC 8878.
//!
//! ## The hash replaces `Dictionary_ID`, and that matters more than it looks
//!
//! RFC 9842 §5: *"The dictionary used for the 'dcz' content encoding is a 'raw'
//! dictionary type."* A raw dictionary carries no `Dictionary_ID` — the format
//! has nowhere to put one — so on its own a raw dictionary cannot tell a
//! decoder it has the wrong one, and wrong bytes come out.
//!
//! `dcz` closes exactly that hole, out of band, with a stronger identifier than
//! the 32-bit ID it replaces. A content-prefix dictionary is therefore not a
//! second-class thing to be upgraded away from on the `dcz` path: it is the
//! shape the transport specifies.

use alloc::vec::Vec;

use zalt_codec::dict::Dictionary;

use crate::error::{Error, ErrorKind, Result};
use crate::sha256::{Digest, Sha256};

/// Length of the `dcz` header, in bytes.
pub const PRELUDE_LEN: usize = 40;

/// The fixed eight bytes that open a `dcz` stream (RFC 9842 §5).
///
/// A skippable-frame magic and a 32-byte length, spelled as the on-disk bytes
/// the spec lists rather than assembled from two integers, so it can be read
/// straight against the document.
pub const PRELUDE_MAGIC: [u8; 8] = [0x5e, 0x2a, 0x4d, 0x18, 0x20, 0x00, 0x00, 0x00];

/// The magic opening a `dcb` stream — Brotli, RFC 9842 §4.
///
/// Recognised only so it can be refused by name. `dcb` is the sibling encoding
/// and arrives through the same negotiation, so a host that muddles its
/// `Accept-Encoding` gets "this is Brotli" rather than "this is not a zstd
/// frame", which is true and sends the reader nowhere.
pub const DCB_MAGIC: [u8; 4] = [0xff, 0x44, 0x43, 0x42];

/// What a stream turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream<'a> {
    /// A `dcz` stream: the dictionary it names, and the frame that follows.
    Dictionary {
        /// SHA-256 of the dictionary this frame was compressed against.
        hash: Digest,
        /// The zstd frame, with the header removed.
        frame: &'a [u8],
    },
    /// No `dcz` header. An ordinary zstd stream, handed back untouched.
    Plain(&'a [u8]),
}

/// Read the header of a stream already known to be `dcz`.
///
/// Use this when `Content-Encoding: dcz` said so. Anything that is not exactly
/// the 40-byte header is an error, because the sender has declared what this
/// is and a mismatch means one of the two ends is wrong.
///
/// Returns the dictionary's hash and the frame that follows it.
pub fn parse_prelude(input: &[u8]) -> Result<(Digest, &[u8])> {
    if input.len() >= 4 && input[..4] == DCB_MAGIC {
        return Err(Error::whole_file(ErrorKind::BrotliDictionaryStream));
    }
    if input.len() < PRELUDE_LEN {
        return Err(Error::whole_file(ErrorKind::Truncated));
    }
    if input[..8] != PRELUDE_MAGIC {
        return Err(Error::whole_file(ErrorKind::NotDczStream));
    }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&input[8..PRELUDE_LEN]);
    Ok((Digest(hash), &input[PRELUDE_LEN..]))
}

/// Decide whether a stream carries a `dcz` header, without being told.
///
/// Use this when sniffing bytes whose encoding was not declared.
///
/// A stream whose first four bytes are the `dcz` magic but whose length field
/// is not 32 is reported as [`Stream::Plain`], deliberately. Those bytes are
/// still a legal zstd skippable frame that `zalt-codec` will step over, and
/// inventing a dictionary requirement the sender never expressed would turn a
/// decodable stream into a failure. Refusing it is [`parse_prelude`]'s job,
/// where the sender *has* said it is `dcz`.
pub fn classify(input: &[u8]) -> Result<Stream<'_>> {
    if input.len() >= 4 && input[..4] == DCB_MAGIC {
        return Err(Error::whole_file(ErrorKind::BrotliDictionaryStream));
    }

    // Not enough bytes to tell yet, but nothing rules it out. A caller
    // streaming from a socket must not commit either way on a short read.
    let known = input.len().min(PRELUDE_MAGIC.len());
    if input[..known] != PRELUDE_MAGIC[..known] {
        return Ok(Stream::Plain(input));
    }
    if input.len() < PRELUDE_LEN {
        return Err(Error::whole_file(ErrorKind::Truncated));
    }

    let mut hash = [0u8; 32];
    hash.copy_from_slice(&input[8..PRELUDE_LEN]);
    Ok(Stream::Dictionary {
        hash: Digest(hash),
        frame: &input[PRELUDE_LEN..],
    })
}

/// Smallest window a `dcz` decoder must support for a dictionary this size.
///
/// RFC 9842 §5: *"Clients that announce support for dcz content encoding MUST
/// be able to decompress resources that were compressed with a window size of
/// at least 8 MB or 1.25 times the size of the dictionary, whichever is
/// greater, up to a maximum of 128 MB."*
///
/// The 1.25 factor is not arbitrary. With zstd the dictionary stays reachable
/// only until the output exceeds the window, so a window comfortably larger
/// than the dictionary is what keeps it addressable for the whole resource.
///
/// Returned as a `windowLog` because that is what [`zalt_codec::params::DecodeLimits`]
/// and every policy in this workspace speak, and rounded **up** to the next
/// power of two: this is a floor a decoder must meet, so rounding down would
/// produce a limit that refuses conforming streams.
pub fn required_window_log(dictionary_len: usize) -> u8 {
    /// 8 MB, the floor. Decimal megabytes, as the RFC writes it.
    const FLOOR: u64 = 8_000_000;
    /// 128 MB, the ceiling.
    const CEILING: u64 = 128_000_000;

    let scaled = (dictionary_len as u64).saturating_mul(5) / 4;
    let needed = scaled.clamp(FLOOR, CEILING);

    // Next power of two at or above `needed`.
    let mut log = 10u8;
    while log < 31 && (1u64 << log) < needed {
        log += 1;
    }
    log
}

/// Dictionaries a host holds, indexed by the hash a `dcz` stream names them by.
///
/// Insertion computes the hash from the bytes, so a dictionary **cannot** be
/// filed under the wrong identity. That is deliberate: the alternative API —
/// insert with a caller-supplied hash, verify later — has a failure mode where
/// the verification is skipped and the wrong dictionary decodes to plausible
/// nonsense. Making it unrepresentable costs one hash per load.
#[derive(Debug, Clone, Default)]
pub struct DictionaryStore {
    entries: Vec<(Digest, Dictionary)>,
}

impl DictionaryStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Hash and add a dictionary, returning the hash a `dcz` stream will name.
    ///
    /// Re-inserting the same bytes replaces the entry rather than duplicating
    /// it; the hash is the identity, so two copies could never disagree.
    pub fn insert(&mut self, raw: &[u8]) -> Result<Digest> {
        let hash = Sha256::digest(raw);
        let dictionary =
            Dictionary::parse(raw).map_err(|_| Error::whole_file(ErrorKind::BadDictionary))?;
        match self.entries.iter_mut().find(|(h, _)| *h == hash) {
            Some(slot) => slot.1 = dictionary,
            None => self.entries.push((hash, dictionary)),
        }
        Ok(hash)
    }

    /// The dictionary with this hash, if it is held.
    pub fn get(&self, hash: &Digest) -> Option<&Dictionary> {
        self.entries.iter().find(|(h, _)| h == hash).map(|(_, d)| d)
    }

    /// Resolve a `dcz` stream to its dictionary and frame in one step.
    ///
    /// `Ok(None)` means the stream is well-formed and names a dictionary this
    /// store does not hold — the ordinary outcome when a client advertised a
    /// dictionary it has since evicted, and a case to retry the request
    /// without `dcz` rather than to fail the resource.
    pub fn resolve<'a>(&self, input: &'a [u8]) -> Result<Option<(&Dictionary, &'a [u8])>> {
        let (hash, frame) = parse_prelude(input)?;
        Ok(self.get(&hash).map(|d| (d, frame)))
    }

    /// Every hash held, in insertion order.
    pub fn hashes(&self) -> impl Iterator<Item = &Digest> {
        self.entries.iter().map(|(h, _)| h)
    }

    /// How many dictionaries are held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn prelude_for(hash: [u8; 32]) -> Vec<u8> {
        let mut v = PRELUDE_MAGIC.to_vec();
        v.extend_from_slice(&hash);
        v
    }

    #[test]
    fn the_magic_is_a_zstd_skippable_frame() {
        // The property the whole design rests on: an unaware decoder skips it.
        let magic = u32::from_le_bytes([
            PRELUDE_MAGIC[0],
            PRELUDE_MAGIC[1],
            PRELUDE_MAGIC[2],
            PRELUDE_MAGIC[3],
        ]);
        assert_eq!(magic, zalt_codec::magic::ZSTD_SKIPPABLE_DCZ);
        assert!(zalt_codec::magic::is_zstd_skippable(magic));

        let len = u32::from_le_bytes([
            PRELUDE_MAGIC[4],
            PRELUDE_MAGIC[5],
            PRELUDE_MAGIC[6],
            PRELUDE_MAGIC[7],
        ]);
        assert_eq!(len, 32, "the skippable frame's payload is the hash");
        assert_eq!(PRELUDE_LEN, 8 + 32);
    }

    #[test]
    fn a_prelude_yields_its_hash_and_the_frame_after_it() {
        let mut input = prelude_for([0xab; 32]);
        input.extend_from_slice(b"FRAME");
        let (hash, frame) = parse_prelude(&input).expect("parses");
        assert_eq!(hash.0, [0xab; 32]);
        assert_eq!(frame, b"FRAME");
    }

    #[test]
    fn a_prelude_truncated_anywhere_is_incomplete_not_malformed() {
        // A socket delivers what it delivers. Every short read below the full
        // header must say "more bytes", never "bad stream" — a decoder that
        // rejects a stream because it arrived in two packets is a bug that
        // only shows up under load.
        let input = prelude_for([0x11; 32]);
        for cut in 0..PRELUDE_LEN {
            assert_eq!(
                parse_prelude(&input[..cut]).unwrap_err().kind,
                ErrorKind::Truncated,
                "cut at {cut}"
            );
        }
        assert!(parse_prelude(&input).is_ok());
    }

    #[test]
    fn classify_says_plain_for_an_ordinary_zstd_frame() {
        let frame = [0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x01, 0x02, 0x03, 0x04];
        assert_eq!(classify(&frame).unwrap(), Stream::Plain(&frame));
    }

    #[test]
    fn classify_waits_rather_than_guessing_on_a_short_read() {
        let input = prelude_for([0x22; 32]);
        // A prefix consistent with the magic is undecided, not Plain: calling
        // it Plain would decode a dictionary stream without its dictionary.
        for cut in 1..PRELUDE_LEN {
            assert_eq!(
                classify(&input[..cut]).unwrap_err().kind,
                ErrorKind::Truncated,
                "cut at {cut}"
            );
        }
        // Zero bytes rule nothing out, so they cannot be classified either.
        // Answering `Plain` here would be a guess, and the caller has not yet
        // given us anything to be right or wrong about.
        assert_eq!(classify(&[]).unwrap_err().kind, ErrorKind::Truncated);
    }

    #[test]
    fn the_dcz_magic_with_a_wrong_length_is_left_alone_when_sniffing() {
        // Still a legal skippable frame; zalt-codec steps over it. Sniffing
        // must not invent a dictionary requirement nobody expressed.
        let mut input = vec![0x5e, 0x2a, 0x4d, 0x18, 0x40, 0x00, 0x00, 0x00];
        input.extend_from_slice(&[0u8; 64]);
        assert!(matches!(classify(&input).unwrap(), Stream::Plain(_)));
        // But when the sender said "dcz", the same bytes are a disagreement.
        assert_eq!(
            parse_prelude(&input).unwrap_err().kind,
            ErrorKind::NotDczStream
        );
    }

    #[test]
    fn a_brotli_dictionary_stream_is_refused_by_name() {
        let mut input = DCB_MAGIC.to_vec();
        input.extend_from_slice(&[0u8; 32]);
        assert_eq!(
            classify(&input).unwrap_err().kind,
            ErrorKind::BrotliDictionaryStream
        );
        assert_eq!(
            parse_prelude(&input).unwrap_err().kind,
            ErrorKind::BrotliDictionaryStream
        );
    }

    #[test]
    fn the_required_window_follows_the_rfc_floor_and_ceiling() {
        // Below the floor the answer is the floor: 8 MB needs windowLog 23,
        // since 2^23 is 8.39 MB and 2^22 is only 4.19 MB.
        assert_eq!(required_window_log(0), 23);
        assert_eq!(required_window_log(47_000), 23);
        // 1.25x takes over once it passes 8 MB.
        assert_eq!(required_window_log(8_000_000), 24);
        assert_eq!(required_window_log(50_000_000), 26);
        // And it is capped at 128 MB however large the dictionary is.
        // windowLog 27 is 134.2 MB, the first power of two above that ceiling.
        assert_eq!(required_window_log(usize::MAX), 27);
    }

    #[test]
    fn a_dictionary_cannot_be_filed_under_the_wrong_hash() {
        let mut store = DictionaryStore::new();
        let raw = b"a raw content-prefix dictionary, which is what dcz specifies";
        let hash = store.insert(raw).expect("raw dictionaries always parse");

        assert_eq!(hash, Sha256::digest(raw));
        assert!(store.get(&hash).is_some());
        assert_eq!(store.get(&hash).unwrap().content(), raw);
        // A raw dictionary carries no id; the hash is the whole identity.
        assert_eq!(store.get(&hash).unwrap().id(), 0);
        assert!(
            store
                .get(&Sha256::digest(b"a different dictionary"))
                .is_none()
        );
    }

    #[test]
    fn reinserting_the_same_bytes_does_not_duplicate() {
        let mut store = DictionaryStore::new();
        let a = store.insert(b"dictionary one").unwrap();
        let b = store.insert(b"dictionary one").unwrap();
        assert_eq!(a, b);
        assert_eq!(store.len(), 1);
        store.insert(b"dictionary two").unwrap();
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn an_unheld_dictionary_resolves_to_none_rather_than_an_error() {
        // The ordinary case when a client advertised a dictionary it has since
        // evicted. That is a retry without `dcz`, not a failed resource.
        let store = DictionaryStore::new();
        let mut input = prelude_for([0x99; 32]);
        input.extend_from_slice(b"FRAME");
        assert!(store.resolve(&input).expect("well formed").is_none());
    }

    #[test]
    fn resolve_returns_the_dictionary_and_the_frame() {
        let mut store = DictionaryStore::new();
        let raw = b"the dictionary bytes";
        let hash = store.insert(raw).unwrap();

        let mut input = prelude_for(hash.0);
        input.extend_from_slice(b"FRAME");
        let (dict, frame) = store.resolve(&input).unwrap().expect("held");
        assert_eq!(dict.content(), raw);
        assert_eq!(frame, b"FRAME");
    }
}
