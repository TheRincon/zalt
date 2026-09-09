//! SHA-256, for the dictionary identity Compression Dictionary Transport uses.
//!
//! The hashing itself is [`sha2`], the RustCrypto implementation. What lives
//! here is a [`Digest`] newtype and the two encodings the transport needs.
//!
//! ## Why this is a dependency and not ninety lines
//!
//! It *was* ninety lines, briefly, on the reasoning that the workspace takes no
//! third-party runtime dependencies and SHA-256 is fully specified with
//! published test vectors. That reasoning was wrong in an instructive way.
//!
//! The hand-written version passed every FIPS 180-4 vector and still dropped
//! buffered bytes whenever an `update` ended without completing a 64-byte
//! block. The published vectors could not catch it: they are all **one-shot**
//! calls, and a one-shot call never re-enters the hasher with a partial buffer.
//! Neither could the million-character vector, whose fixed 1 000-byte updates
//! never leave the buffer short. It took a test hashing every length from 0 to
//! 130 one byte at a time.
//!
//! So the algorithm was never the risk; the plumbing around it was, and
//! standard vectors verify the algorithm. `block-buffer` — the crate whose
//! entire job is that plumbing — is worth having.
//!
//! This is the **first third-party runtime dependency in the workspace**, and a
//! deliberate exception rather than a change of policy. The manifest parser and
//! the base64 below are still hand-written, because a wrong TOML parse fails
//! loudly at load and a wrong base64 produces visibly broken ASCII, while a
//! wrong hash silently selects the wrong dictionary and decodes to plausible
//! nonsense. The rule that survives is about the *consequence of being wrong*,
//! not about dependency count.

use core::fmt;

use sha2::Digest as _;

/// A 32-byte SHA-256 digest.
///
/// The identifier a `dcz` stream uses to name its dictionary.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest(pub [u8; 32]);

impl Digest {
    /// The raw bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hex, for diagnostics and for the provenance files.
    pub fn to_hex(&self) -> [u8; 64] {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = [0u8; 64];
        for (i, byte) in self.0.iter().enumerate() {
            out[i * 2] = HEX[(byte >> 4) as usize];
            out[i * 2 + 1] = HEX[(byte & 0x0f) as usize];
        }
        out
    }

    /// Standard base64, the encoding `Available-Dictionary` carries.
    ///
    /// RFC 9842 has the client announce a dictionary it holds as a structured
    /// field byte sequence — `Available-Dictionary: :<base64>:`. This produces
    /// the inner text only; building the header, and everything else about
    /// HTTP, is the host's business and deliberately not this crate's.
    ///
    /// Hand-written where the hash is not, and the distinction is the point: a
    /// base64 bug corrupts an ASCII string a server rejects immediately, where
    /// a hash bug is silent. The input is also always exactly 32 bytes, so the
    /// one length that can occur is the one the test covers.
    ///
    /// 32 bytes is not a multiple of 3, so the output carries one `=` pad.
    pub fn to_base64(&self) -> [u8; 44] {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = [b'='; 44];
        let mut o = 0;
        for chunk in self.0.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out[o] = ALPHABET[(n >> 18) as usize & 63];
            out[o + 1] = ALPHABET[(n >> 12) as usize & 63];
            if chunk.len() > 1 {
                out[o + 2] = ALPHABET[(n >> 6) as usize & 63];
            }
            if chunk.len() > 2 {
                out[o + 3] = ALPHABET[n as usize & 63];
            }
            o += 4;
        }
        out
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hex = self.to_hex();
        // Every byte comes from a hex alphabet, so this cannot fail; written
        // without `unwrap` so the crate stays panic-free on this path.
        match core::str::from_utf8(&hex) {
            Ok(s) => f.write_str(s),
            Err(_) => f.write_str("<invalid digest>"),
        }
    }
}

/// Streaming SHA-256.
///
/// A thin wrapper over [`sha2::Sha256`], kept so the rest of the crate speaks
/// [`Digest`] rather than a `GenericArray`, and so replacing the backing
/// implementation stays a one-file change.
#[derive(Clone, Default)]
pub struct Sha256 {
    inner: sha2::Sha256,
}

impl Sha256 {
    /// A fresh hasher.
    pub fn new() -> Self {
        Self::default()
    }

    /// Hash a complete message in one call.
    pub fn digest(data: &[u8]) -> Digest {
        Digest(sha2::Sha256::digest(data).into())
    }

    /// Add more of the message.
    pub fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }

    /// Finish and produce the digest.
    pub fn finish(self) -> Digest {
        Digest(self.inner.finalize().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec;

    fn hex(d: &Digest) -> String {
        String::from_utf8(d.to_hex().to_vec()).expect("hex is ascii")
    }

    /// FIPS 180-4 / NIST CAVP vectors.
    ///
    /// Kept after the swap to `sha2`. They no longer guard an implementation we
    /// wrote, but they do guard the wiring: a `.into()` that transposed bytes,
    /// or a later swap to a different backend, fails here.
    #[test]
    fn nist_vectors() {
        let cases: &[(&[u8], &str)] = &[
            (
                b"",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                b"abc",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
            (
                b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu",
                "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(hex(&Sha256::digest(input)), *expected, "{input:?}");
        }
    }

    #[test]
    fn a_million_a_characters() {
        let mut h = Sha256::new();
        for _ in 0..1000 {
            h.update(&[b'a'; 1000]);
        }
        assert_eq!(
            hex(&h.finish()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    /// The test that found the bug in the hand-written version.
    ///
    /// Kept deliberately. It is worth little against `sha2` and was worth the
    /// whole exercise against ours — a standing reminder of which half of a
    /// hash implementation published vectors actually cover.
    #[test]
    fn every_length_across_two_blocks_is_consistent() {
        let data: vec::Vec<u8> = (0..130u32).map(|i| (i * 7 % 251) as u8).collect();
        for n in 0..=130usize {
            let mut h = Sha256::new();
            for byte in &data[..n] {
                h.update(core::slice::from_ref(byte));
            }
            assert_eq!(h.finish(), Sha256::digest(&data[..n]), "length {n}");
        }
    }

    #[test]
    fn streaming_in_any_chunking_matches_one_shot() {
        let data: vec::Vec<u8> = (0..300u32).map(|i| (i % 251) as u8).collect();
        let once = Sha256::digest(&data);
        for split in [0, 1, 55, 56, 57, 63, 64, 65, 127, 128, 129, 200, 300] {
            let (a, b) = data.split_at(split.min(data.len()));
            let mut h = Sha256::new();
            h.update(a);
            h.update(b);
            assert_eq!(h.finish(), once, "split at {split}");
        }
    }

    #[test]
    fn base64_matches_the_known_encoding_of_the_empty_digest() {
        // The empty-string digest is the most-published SHA-256 in existence,
        // and its base64 is what an `Available-Dictionary` header would carry.
        let d = Sha256::digest(b"");
        assert_eq!(
            core::str::from_utf8(&d.to_base64()).unwrap(),
            "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="
        );
    }
}
