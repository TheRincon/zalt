//! Frame headers (RFC 8878 §3.1.1) and skippable frames (§3.1.2).
//!
//! Parsing is split in two because the header's *length* depends on its first
//! byte: [`header_len_after_descriptor`] says how much more to read, and
//! [`FrameHeader::parse`] interprets it once that much is available. A streaming
//! decoder cannot know up front how many bytes a frame header occupies.

use crate::error::{Error, Result};
use crate::magic;
use crate::params::{self, DecodeLimits};

/// Largest possible frame header, in bytes, including the magic number.
///
/// magic 4 + descriptor 1 + window 1 + dictionary id 4 + content size 8.
pub const MAX_FRAME_HEADER: usize = 18;

/// Largest block a frame may contain, before the window is considered.
pub const BLOCK_SIZE_ABSOLUTE_MAX: u32 = 128 * 1024;

/// A parsed frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Back-reference limit, and therefore the history the decoder must retain.
    pub window_size: u64,
    /// Declared size of the decoded content, when the frame declares one.
    pub frame_content_size: Option<u64>,
    /// Dictionary required to decode this frame, if any.
    pub dictionary_id: Option<u32>,
    /// Whether a 4-byte content checksum follows the last block.
    pub has_checksum: bool,
    /// Whether the frame is a single segment (no window descriptor).
    pub single_segment: bool,
}

/// Interpretation of the `Frame_Header_Descriptor` byte.
#[derive(Debug, Clone, Copy)]
struct Descriptor {
    fcs_flag: u8,
    single_segment: bool,
    has_checksum: bool,
    did_flag: u8,
}

impl Descriptor {
    fn parse(byte: u8) -> Result<Self> {
        // Bit 3 is Reserved_bit: "must be zero. A decoder compliant with this
        // specification version must ensure it is not set."
        if byte & 0b0000_1000 != 0 {
            return Err(Error::Reserved);
        }
        // Bit 4 is Unused_bit. The spec says a compliant decoder "shall not
        // interpret it" — so it is deliberately NOT checked. Rejecting on it
        // would break forward compatibility with a future revision that
        // assigns it, which is exactly what the spec is reserving the right
        // to do.
        Ok(Self {
            fcs_flag: byte >> 6,
            single_segment: byte & 0b0010_0000 != 0,
            has_checksum: byte & 0b0000_0100 != 0,
            did_flag: byte & 0b0000_0011,
        })
    }

    fn window_descriptor_size(&self) -> usize {
        usize::from(!self.single_segment)
    }

    fn dictionary_id_size(&self) -> usize {
        match self.did_flag {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 4,
        }
    }

    fn content_size_field_size(&self) -> usize {
        match self.fcs_flag {
            // The one asymmetric case: with no window descriptor, the content
            // size doubles as the window size, so it must be present.
            0 if self.single_segment => 1,
            0 => 0,
            n => 1 << n,
        }
    }
}

/// How many bytes follow the descriptor byte in this frame header.
pub fn header_len_after_descriptor(descriptor: u8) -> Result<usize> {
    let d = Descriptor::parse(descriptor)?;
    Ok(d.window_descriptor_size() + d.dictionary_id_size() + d.content_size_field_size())
}

impl FrameHeader {
    /// Parse a frame header from the descriptor byte and the bytes following it.
    ///
    /// `rest` must be exactly [`header_len_after_descriptor`] bytes.
    pub fn parse(descriptor: u8, rest: &[u8], limits: &DecodeLimits) -> Result<Self> {
        let d = Descriptor::parse(descriptor)?;
        if rest.len() != header_len_after_descriptor(descriptor)? {
            return Err(Error::Malformed);
        }

        let mut cursor = 0usize;

        let window_descriptor = if d.single_segment {
            None
        } else {
            let b = rest[cursor];
            cursor += 1;
            Some(b)
        };

        let dictionary_id = match d.dictionary_id_size() {
            0 => None,
            n => {
                let mut v = 0u32;
                for i in 0..n {
                    v |= u32::from(rest[cursor + i]) << (8 * i);
                }
                cursor += n;
                // A zero dictionary ID means "no dictionary", even when the
                // field is present.
                (v != 0).then_some(v)
            }
        };

        let frame_content_size = match d.content_size_field_size() {
            0 => None,
            1 => Some(u64::from(rest[cursor])),
            // The spec's one arithmetic quirk: with a 2-byte field, 256 is
            // added to the stored value. Forgetting this decodes short frames
            // as 256 bytes too small, which looks like corrupt data much
            // further along.
            2 => Some(u64::from(u16::from_le_bytes([rest[cursor], rest[cursor + 1]])) + 256),
            4 => Some(u64::from(u32::from_le_bytes(
                rest[cursor..cursor + 4]
                    .try_into()
                    .map_err(|_| Error::Malformed)?,
            ))),
            8 => Some(u64::from_le_bytes(
                rest[cursor..cursor + 8]
                    .try_into()
                    .map_err(|_| Error::Malformed)?,
            )),
            _ => return Err(Error::Malformed),
        };

        // Window size, and the policy check, before anything is allocated.
        let window_size = match window_descriptor {
            Some(b) => {
                limits.check_window_log(params::window_log_from_descriptor(b))?;
                params::window_size_from_descriptor(b)?
            }
            None => {
                // Single-segment: the whole content must be held at once, so
                // the content size *is* the window. This is the second route
                // to a large allocation, and capping only the window
                // descriptor would leave it open — the frame content size is
                // a u64.
                let fcs = frame_content_size.ok_or(Error::Malformed)?;
                let implied_log = 64 - fcs.max(1).leading_zeros();
                limits.check_window_log(implied_log as u8)?;
                fcs
            }
        };

        if let (Some(limit), Some(declared)) = (limits.max_decoded_size, frame_content_size)
            && declared > limit
        {
            return Err(Error::TooLarge);
        }

        Ok(Self {
            window_size,
            frame_content_size,
            dictionary_id,
            has_checksum: d.has_checksum,
            single_segment: d.single_segment,
        })
    }

    /// Largest block this frame may contain: `min(128 KiB, window_size)`.
    pub fn block_size_max(&self) -> u64 {
        u64::from(BLOCK_SIZE_ABSOLUTE_MAX).min(self.window_size)
    }
}

/// Classification of a 4-byte magic number at the start of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// A standard zstd frame.
    Zstd,
    /// A skippable frame, whose 4-byte length follows.
    Skippable,
}

/// Classify a frame magic number.
pub fn classify(magic_number: u32) -> Result<FrameKind> {
    if magic_number == magic::ZSTD_FRAME {
        Ok(FrameKind::Zstd)
    } else if magic::is_zstd_skippable(magic_number) {
        Ok(FrameKind::Skippable)
    } else {
        // Legacy zstd frames (v0.1–v0.7) land here too, deliberately. They are
        // rejected by design, not by omission.
        Err(Error::BadMagic)
    }
}

/// The on-disk length of the frame — zstd or skippable — that starts at
/// `src[0]`, found from its headers alone, without decoding anything.
///
/// A zstd frame is walked header, then block header by block header to the
/// last block, then its checksum: RFC 8878 makes every frame self-delimiting
/// this way, which is what lets a damaged file be walked frame by frame
/// (ZLTA §11.3). libzstd's `ZSTD_findFrameCompressedSize` does the same.
///
/// [`Error::Truncated`] if `src` ends before the frame does — the answer for
/// a file cut short, and also for a slice that is merely too small yet, so a
/// caller reading a file in pieces can read more and ask again.
/// Window and content-size limits are not applied: this only measures.
pub fn compressed_size(src: &[u8]) -> Result<usize> {
    let magic_number = u32::from_le_bytes(*src.first_chunk::<4>().ok_or(Error::Truncated)?);
    if classify(magic_number)? == FrameKind::Skippable {
        let size = src.get(4..8).ok_or(Error::Truncated)?;
        let user = u32::from_le_bytes(size.try_into().map_err(|_| Error::Malformed)?) as usize;
        let total = 8usize.checked_add(user).ok_or(Error::Malformed)?;
        return if total <= src.len() {
            Ok(total)
        } else {
            Err(Error::Truncated)
        };
    }
    let descriptor = *src.get(4).ok_or(Error::Truncated)?;
    let d = Descriptor::parse(descriptor)?;
    let mut pos = 5 + header_len_after_descriptor(descriptor)?;
    loop {
        let header = src
            .get(pos..pos + crate::block::BLOCK_HEADER_SIZE)
            .ok_or(Error::Truncated)?;
        let block = crate::block::BlockHeader::parse([header[0], header[1], header[2]])?;
        pos += crate::block::BLOCK_HEADER_SIZE + block.stream_len() as usize;
        if pos > src.len() {
            return Err(Error::Truncated);
        }
        if block.last {
            break;
        }
    }
    if d.has_checksum {
        pos += 4;
    }
    if pos > src.len() {
        return Err(Error::Truncated);
    }
    Ok(pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn permissive() -> DecodeLimits {
        DecodeLimits::default()
    }

    /// Descriptor 0x24, taken from real `zstd -1` output for empty input:
    /// single segment, checksum present, 1-byte content size.
    /// Frames built by hand, so every field the walk reads is one the test
    /// chose: header widths, each block type, the checksum.
    fn frame(
        descriptor: u8,
        header_rest: &[u8],
        blocks: &[(u32, u32, &[u8])],
        checksum: bool,
    ) -> alloc::vec::Vec<u8> {
        let mut f = magic::to_le_bytes(magic::ZSTD_FRAME).to_vec();
        f.push(descriptor | if checksum { 0b100 } else { 0 });
        f.extend_from_slice(header_rest);
        for (i, (block_type, size, body)) in blocks.iter().enumerate() {
            let last = u32::from(i + 1 == blocks.len());
            let raw = last | (block_type << 1) | (size << 3);
            f.extend_from_slice(&raw.to_le_bytes()[..3]);
            f.extend_from_slice(body);
        }
        if checksum {
            f.extend_from_slice(&[1, 2, 3, 4]);
        }
        f
    }

    #[test]
    fn compressed_size_walks_every_block_type_and_the_checksum() {
        // Single segment, 1-byte content size; a raw block of 3, an RLE block
        // of 1000 (one byte on the wire), a "compressed" block of 2 bytes.
        let f = frame(
            0x20,
            &[0x05],
            &[(0, 3, b"abc"), (1, 1000, b"x"), (2, 2, b"zz")],
            true,
        );
        assert_eq!(compressed_size(&f), Ok(f.len()));
        // Trailing bytes are the next frame's business.
        let mut more = f.clone();
        more.extend_from_slice(b"next frame");
        assert_eq!(compressed_size(&more), Ok(f.len()));
        // Every cut is Truncated, never a wrong length.
        for cut in 0..f.len() {
            assert_eq!(
                compressed_size(&f[..cut]),
                Err(Error::Truncated),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn compressed_size_reads_the_header_widths() {
        // Window descriptor, 4-byte dictionary id, 8-byte content size.
        let f = frame(
            0b1100_0011,
            &[0x00, 1, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0],
            &[(0, 1, b"q")],
            false,
        );
        assert_eq!(compressed_size(&f), Ok(f.len()));
    }

    #[test]
    fn compressed_size_of_skippable_frames_and_refusals() {
        let mut s = magic::to_le_bytes(0x184D_2A5E).to_vec();
        s.extend_from_slice(&3u32.to_le_bytes());
        s.extend_from_slice(b"abc");
        assert_eq!(compressed_size(&s), Ok(11));
        assert_eq!(compressed_size(&s[..10]), Err(Error::Truncated));
        assert_eq!(
            compressed_size(b"\x00\x01\x02\x03\x04"),
            Err(Error::BadMagic)
        );
        let reserved_block = frame(0x20, &[0x05], &[(3, 1, b"x")], false);
        assert_eq!(compressed_size(&reserved_block), Err(Error::Reserved));
        let reserved_bit = frame(0x28, &[0x05], &[(0, 1, b"x")], false);
        assert_eq!(compressed_size(&reserved_bit), Err(Error::Reserved));
    }

    #[test]
    fn compressed_size_matches_the_encoder_on_real_frames() {
        let data: alloc::vec::Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 13) as u8).collect();
        for level in [1, 3, 19] {
            let params = crate::EncodeParams {
                level,
                ..crate::EncodeParams::default()
            };
            let f = crate::compress(&data, &params).unwrap();
            assert_eq!(compressed_size(&f), Ok(f.len()), "level {level}");
        }
    }

    #[test]
    fn descriptor_0x24_from_real_zstd() {
        assert_eq!(header_len_after_descriptor(0x24).unwrap(), 1);
        let h = FrameHeader::parse(0x24, &[0x00], &permissive()).unwrap();
        assert_eq!(h.frame_content_size, Some(0));
        assert_eq!(h.window_size, 0);
        assert!(h.has_checksum);
        assert!(h.single_segment);
        assert_eq!(h.dictionary_id, None);
    }

    /// Descriptor 0x64 with content size `00 0f`, from real `zstd -1` output
    /// on 4096 bytes: the 2-byte field carries 3840 and means 4096.
    #[test]
    fn two_byte_content_size_adds_256() {
        assert_eq!(header_len_after_descriptor(0x64).unwrap(), 2);
        let h = FrameHeader::parse(0x64, &[0x00, 0x0f], &permissive()).unwrap();
        assert_eq!(h.frame_content_size, Some(4096));
        assert_eq!(h.window_size, 4096, "single segment: window is the content");
    }

    #[test]
    fn reserved_bit_is_rejected() {
        assert!(matches!(
            Descriptor::parse(0b0000_1000).map(|_| ()),
            Err(Error::Reserved)
        ));
    }

    /// Bit 4 is "unused", and the spec forbids interpreting it. Rejecting on it
    /// would break against a future revision that assigns it.
    #[test]
    fn unused_bit_is_not_rejected() {
        assert!(Descriptor::parse(0b0001_0000).is_ok());
    }

    #[test]
    fn window_descriptor_path() {
        // Not single-segment: one window descriptor byte, no content size.
        let desc = 0b0000_0000;
        assert_eq!(header_len_after_descriptor(desc).unwrap(), 1);
        let h = FrameHeader::parse(desc, &[0x00], &permissive()).unwrap();
        assert_eq!(h.window_size, 1024);
        assert_eq!(h.frame_content_size, None);
        assert!(!h.has_checksum);
    }

    #[test]
    fn dictionary_id_widths() {
        for (flag, width) in [(0u8, 0usize), (1, 1), (2, 2), (3, 4)] {
            let d = Descriptor::parse(flag).unwrap();
            assert_eq!(d.dictionary_id_size(), width);
        }
    }

    #[test]
    fn zero_dictionary_id_means_none() {
        // did_flag 1, one zero byte, plus a window descriptor.
        let h = FrameHeader::parse(0b0000_0001, &[0x00, 0x00], &permissive()).unwrap();
        assert_eq!(h.dictionary_id, None);
    }

    #[test]
    fn nonzero_dictionary_id_is_reported() {
        let h = FrameHeader::parse(0b0000_0001, &[0x00, 0x2A], &permissive()).unwrap();
        assert_eq!(h.dictionary_id, Some(0x2A));
    }

    /// The amplification header: `windowLog` 41 from a single descriptor byte.
    #[test]
    fn oversized_window_is_rejected_at_header_parse() {
        let limits = DecodeLimits {
            max_window_log: 23,
            ..Default::default()
        };
        assert!(matches!(
            FrameHeader::parse(0b0000_0000, &[0xFF], &limits),
            Err(Error::WindowTooLarge { .. })
        ));
    }

    /// The second route to the same place, and the one easy to miss: with
    /// `Single_Segment_flag` there is no window descriptor at all, and the
    /// u64 content size becomes the window.
    #[test]
    fn single_segment_content_size_is_also_capped() {
        let limits = DecodeLimits {
            max_window_log: 23,
            ..Default::default()
        };
        // fcs_flag 3 -> 8-byte content size, single segment.
        let desc = 0b1110_0000;
        assert_eq!(header_len_after_descriptor(desc).unwrap(), 8);
        let huge = u64::to_le_bytes(1 << 40);
        assert!(
            matches!(
                FrameHeader::parse(desc, &huge, &limits),
                Err(Error::WindowTooLarge { .. })
            ),
            "a 1 TiB single-segment frame must be refused like a 1 TiB window"
        );
    }

    #[test]
    fn single_segment_within_cap_is_accepted() {
        let limits = DecodeLimits {
            max_window_log: 23,
            ..Default::default()
        };
        let desc = 0b1110_0000;
        let ok = u64::to_le_bytes(1 << 20);
        let h = FrameHeader::parse(desc, &ok, &limits).unwrap();
        assert_eq!(h.window_size, 1 << 20);
    }

    #[test]
    fn classify_recognises_the_three_cases() {
        assert_eq!(classify(magic::ZSTD_FRAME).unwrap(), FrameKind::Zstd);
        assert_eq!(
            classify(magic::ZSTD_SKIPPABLE_DCZ).unwrap(),
            FrameKind::Skippable
        );
        assert!(matches!(classify(0xDEAD_BEEF), Err(Error::BadMagic)));
    }

    #[test]
    fn block_size_max_is_bounded_by_the_window() {
        let small = FrameHeader {
            window_size: 1024,
            frame_content_size: None,
            dictionary_id: None,
            has_checksum: false,
            single_segment: false,
        };
        assert_eq!(small.block_size_max(), 1024);

        let large = FrameHeader {
            window_size: 8 << 20,
            ..small
        };
        assert_eq!(large.block_size_max(), u64::from(BLOCK_SIZE_ABSOLUTE_MAX));
    }
}
