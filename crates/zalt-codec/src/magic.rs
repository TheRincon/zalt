//! Four-byte signature registry.
//!
//! Every magic number zalt recognises or emits lives here, in one place, so
//! that [`tests`] can assert they cannot collide. A magic number invented in
//! the module that happens to need it is a magic number nobody checks against
//! the others.
//!
//! All values are the **little-endian** interpretation of the first four bytes
//! on the wire, matching how RFC 8878 specifies them.
//!
//! # The boundary
//!
//! zalt does **not** define a magic number for zstd frames. Frames on the wire
//! are [`ZSTD_FRAME`] and nothing else; inventing an alternative would forfeit
//! the interoperability that is the entire reason the wire path is spec-exact.
//! [`ZALT_NATIVE`] identifies our *own* container — the cache codec, where we
//! own both ends — and never appears in anything served as
//! `Content-Encoding: zstd`.

/// Zstandard frame magic (RFC 8878 §3.1.1).
///
/// On disk, little-endian: `28 B5 2F FD`.
pub const ZSTD_FRAME: u32 = 0xFD2F_B528;

/// Zstandard dictionary magic (RFC 8878 §5).
///
/// On disk, little-endian: `37 A4 30 EC`.
pub const ZSTD_DICTIONARY: u32 = 0xEC30_A437;

/// First magic in the skippable-frame range (RFC 8878 §3.1.2).
pub const ZSTD_SKIPPABLE_START: u32 = 0x184D_2A50;

/// Last magic in the skippable-frame range.
pub const ZSTD_SKIPPABLE_END: u32 = 0x184D_2A5F;

/// Mask isolating the fixed part of a skippable-frame magic.
pub const ZSTD_SKIPPABLE_MASK: u32 = 0xFFFF_FFF0;

/// The skippable magic used by Compression Dictionary Transport (`dcz`).
///
/// A `dcz` payload is this magic, length `0x20`, the dictionary's 32-byte
/// SHA-256, then an ordinary zstd frame. Handled in M4, but named here so the
/// value is registered rather than rediscovered.
pub const ZSTD_SKIPPABLE_DCZ: u32 = 0x184D_2A5E;

/// zalt native container magic — the cache codec (M6).
///
/// On disk, little-endian: `5A 4C 54 FD` — `Z`, `L`, `T`, `0xFD`.
///
/// The shape is deliberate, and copies zstd's reasoning rather than its value:
///
/// - The leading byte is printable ASCII `Z`, so `hexdump` and `file` output is
///   recognisable to a human debugging a cache directory at 2am.
/// - The trailing byte `0xFD` is **never valid in UTF-8** (`0xF5`–`0xFF` cannot
///   begin any sequence). This is the same trick zstd plays with its own
///   trailing `0xFD`: it stops a container from being sniffed as text, and
///   makes accidental concatenation into a text stream detectable.
/// - It is far from [`ZSTD_FRAME`] in every byte, so a truncated or misrouted
///   file cannot be mistaken for the other format by a partial comparison.
///
/// This value is only meaningful once the native codec exists. It is defined
/// now so the registry is complete and the collision test covers it.
pub const ZALT_NATIVE: u32 = 0xFD54_4C5A;

/// Does this magic begin a zstd skippable frame?
///
/// Skippable frames must be recognised and stepped over by any conformant
/// decoder — `dcz` dictionary transport rides in one.
#[inline]
pub const fn is_zstd_skippable(magic: u32) -> bool {
    magic & ZSTD_SKIPPABLE_MASK == ZSTD_SKIPPABLE_START
}

/// The four on-disk bytes of a magic number.
#[inline]
pub const fn to_le_bytes(magic: u32) -> [u8; 4] {
    magic.to_le_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: &[(&str, u32)] = &[
        ("ZSTD_FRAME", ZSTD_FRAME),
        ("ZSTD_DICTIONARY", ZSTD_DICTIONARY),
        ("ZSTD_SKIPPABLE_START", ZSTD_SKIPPABLE_START),
        ("ZSTD_SKIPPABLE_END", ZSTD_SKIPPABLE_END),
        ("ZALT_NATIVE", ZALT_NATIVE),
    ];

    /// The reason this module exists.
    #[test]
    fn no_two_signatures_collide() {
        for (i, (name_a, a)) in ALL.iter().enumerate() {
            for (name_b, b) in &ALL[i + 1..] {
                assert_ne!(a, b, "{name_a} collides with {name_b}");
            }
        }
    }

    #[test]
    fn zalt_native_is_not_a_zstd_signature() {
        assert_ne!(ZALT_NATIVE, ZSTD_FRAME);
        assert_ne!(ZALT_NATIVE, ZSTD_DICTIONARY);
        assert!(!is_zstd_skippable(ZALT_NATIVE));
    }

    #[test]
    fn zalt_native_on_disk_layout() {
        assert_eq!(to_le_bytes(ZALT_NATIVE), [b'Z', b'L', b'T', 0xFD]);
    }

    /// A container must never be sniffable as text.
    #[test]
    fn zalt_native_is_invalid_utf8() {
        let bytes = to_le_bytes(ZALT_NATIVE);
        assert!(core::str::from_utf8(&bytes).is_err());
        // 0xF5..=0xFF can never begin a UTF-8 sequence, so this holds no matter
        // what follows the magic in the stream.
        assert!(bytes[3] >= 0xF5);
    }

    #[test]
    fn zstd_frame_magic_on_disk_layout() {
        assert_eq!(to_le_bytes(ZSTD_FRAME), [0x28, 0xB5, 0x2F, 0xFD]);
    }

    #[test]
    fn skippable_range_recognised_exactly() {
        for m in ZSTD_SKIPPABLE_START..=ZSTD_SKIPPABLE_END {
            assert!(is_zstd_skippable(m), "{m:#X} should be skippable");
        }
        assert!(!is_zstd_skippable(ZSTD_SKIPPABLE_START - 1));
        assert!(!is_zstd_skippable(ZSTD_SKIPPABLE_END + 1));
        assert!(!is_zstd_skippable(ZSTD_FRAME));
    }

    #[test]
    fn dcz_is_inside_the_skippable_range() {
        assert!(is_zstd_skippable(ZSTD_SKIPPABLE_DCZ));
    }

    /// Guards against a "small tweak" that quietly makes the two formats
    /// confusable after a truncated write.
    #[test]
    fn zalt_and_zstd_differ_in_every_byte() {
        let a = to_le_bytes(ZALT_NATIVE);
        let b = to_le_bytes(ZSTD_FRAME);
        for i in 0..3 {
            assert_ne!(a[i], b[i], "byte {i} matches between ZALT and ZSTD");
        }
    }
}
