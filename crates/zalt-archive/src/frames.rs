//! Where the archive meets the codec: one frame in, one frame out.
//!
//! Each direction enforces a rule the spec states and the codec has no reason
//! to know about, rather than relying on the codec happening to satisfy it.

use zalt_codec::decode::{Decoder, Status};
use zalt_codec::encode::{EncodeParams, Strategy};
use zalt_codec::frame::{FrameHeader, header_len_after_descriptor};
use zalt_codec::params::DecodeLimits;

use crate::layout::{is_zstd_frame, le32};
use crate::{Error, Result, format_err};

/// Largest decoded frame §5.2 allows: 1 GiB.
pub const MAX_FRAME_CONTENT: u64 = 1 << 30;

/// Largest window §5.2 allows a data frame to require: 2^27 (128 MiB).
///
/// Above zalt's own untrusted-decode ceiling of 2^23, deliberately. A reader
/// that refused 2^27 would refuse conformant archives written by `zstd --ultra
/// -22`; the cost of allowing it is bounded by the reader's frame limits, not
/// by a smaller window.
pub const MAX_WINDOW_LOG: u8 = 27;

/// Compress `content` as one standalone frame, as §5.2 requires a data frame
/// to be: a content checksum, and `Frame_Content_Size` present.
///
/// The second is *checked*, not assumed. zalt's frame writer omits the field
/// for some frames under 256 bytes; every frame an archive holds is either a
/// multiple of 512 or a single-segment index body, so it is always present
/// today — but that is a coupling between two crates, and a frame silently
/// missing its size would be refused by every conformant ZSTA reader.
pub fn encode(content: &[u8], level: i32) -> Result<Vec<u8>> {
    let params = EncodeParams {
        strategy: Strategy::Compress,
        level,
        checksum: true,
        ..Default::default()
    };
    let frame = zalt_embed::compress(content, &params)?;
    let header = header_of(&frame, &DecodeLimits::default())?;
    if header.frame_content_size != Some(content.len() as u64) {
        return Err(Error::Format(format!(
            "the encoder omitted Frame_Content_Size for a {}-byte frame (§5.2 rule 2)",
            content.len()
        )));
    }
    Ok(frame)
}

/// Parse a zstd frame's header.
pub fn header_of(frame: &[u8], limits: &DecodeLimits) -> Result<FrameHeader> {
    if frame.len() < 5 || !is_zstd_frame(le32(frame)) {
        return format_err("expected a Zstandard frame");
    }
    let rest_len = header_len_after_descriptor(frame[4])?;
    if frame.len() < 5 + rest_len {
        return format_err("truncated frame header");
    }
    Ok(FrameHeader::parse(
        frame[4],
        &frame[5..5 + rest_len],
        limits,
    )?)
}

/// Decode `frame`, which must be exactly one Zstandard frame of exactly
/// `expected` decoded bytes — no more frames after it, nothing left over.
///
/// The "exactly one" matters because a stock decoder would happily read a
/// skippable frame tucked in after it, and so would any reader that decoded the
/// region as a stream. The seek table says this region *is* one frame; a
/// region that is not is a damaged or doctored archive, and should read as one.
pub fn decode_exactly(frame: &[u8], expected: u64) -> Result<Vec<u8>> {
    if expected > MAX_FRAME_CONTENT {
        return Err(Error::Refused(format!(
            "a frame of {expected} bytes is over §5.2's 1 GiB limit"
        )));
    }
    let limits = DecodeLimits {
        max_window_log: MAX_WINDOW_LOG,
        max_decoded_size: Some(expected),
    };
    let header = header_of(frame, &limits)?;
    if header.frame_content_size != Some(expected) {
        return format_err(format!(
            "frame declares {:?} bytes where the seek table says {expected}",
            header.frame_content_size
        ));
    }
    if header.dictionary_id.is_some() {
        return Err(Error::Unsupported(
            "dictionary-compressed frames (§6)".into(),
        ));
    }

    let mut decoder = Decoder::<zalt_embed::DefaultBackend>::new(limits);
    let mut out = Vec::with_capacity(expected as usize);
    let mut cursor = 0usize;
    loop {
        let progress = decoder.decompress(&frame[cursor..], &mut out)?;
        cursor += progress.consumed;
        match progress.status {
            Status::FrameComplete => break,
            Status::NeedMoreInput if cursor >= frame.len() => {
                return format_err("frame ends before its last block");
            }
            Status::NeedMoreInput if progress.consumed == 0 => {
                return format_err("frame decoding made no progress");
            }
            Status::NeedMoreInput => {}
        }
    }
    if cursor != frame.len() {
        return format_err(format!(
            "{} bytes follow the frame inside its seek table entry",
            frame.len() - cursor
        ));
    }
    if out.len() as u64 != expected {
        return format_err("frame decoded to a different length than it declared");
    }
    Ok(out)
}

/// Low 32 bits of XXH64 (seed 0): the seek table's per-frame checksum.
pub fn seek_checksum(content: &[u8]) -> u32 {
    zalt_codec::xxh64::xxh64(content, 0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_round_trips_and_declares_its_size() {
        for len in [0usize, 1, 255, 256, 512, 1024, 300_000] {
            let content: Vec<u8> = (0..len).map(|i| (i % 7) as u8).collect();
            let frame = encode(&content, 3).unwrap();
            assert_eq!(
                decode_exactly(&frame, len as u64).unwrap(),
                content,
                "{len}"
            );
        }
    }

    #[test]
    fn anything_after_the_frame_is_refused() {
        let frame = encode(&[0u8; 1024], 3).unwrap();
        let mut padded = frame.clone();
        // A skippable frame a stock decoder would step over without comment.
        padded.extend_from_slice(&[0x50, 0x2A, 0x4D, 0x18, 0, 0, 0, 0]);
        assert!(decode_exactly(&padded, 1024).is_err());
        let mut doubled = frame.clone();
        doubled.extend_from_slice(&frame);
        assert!(decode_exactly(&doubled, 1024).is_err());
        assert!(decode_exactly(&frame[..frame.len() - 1], 1024).is_err());
    }

    #[test]
    fn the_declared_size_must_be_the_seek_tables() {
        let frame = encode(&[0u8; 1024], 3).unwrap();
        assert!(decode_exactly(&frame, 512).is_err());
        assert!(decode_exactly(&frame, 2048).is_err());
    }
}
