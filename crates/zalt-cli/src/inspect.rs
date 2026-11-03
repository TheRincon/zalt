//! `zalt info` — what is in this file, without decoding it.
//!
//! Block sizes are carried in block headers, so the whole structure of a frame
//! can be walked by skipping from header to header without decompressing a
//! single byte. That makes this cheap enough to run over a corpus, and useful
//! for exactly the questions that come up while reading RFC 8878 alongside a
//! real file: how many blocks, of what types, how big, and did the encoder emit
//! a checksum.
//!
//! Unlike the decode path this reads the whole input into memory. It is an
//! inspection tool pointed at a file, not something on a hostile network path,
//! and streaming it would buy nothing but complexity.

use std::fmt::Write as _;

use zalt_codec::block::{BLOCK_HEADER_SIZE, BlockHeader, BlockType};
use zalt_codec::frame::{self, FrameKind};
use zalt_codec::params::DecodeLimits;
use zalt_codec::{Error, FrameHeader};

use crate::args::human;

/// One frame's structure.
#[derive(Debug)]
pub enum Frame {
    /// An ordinary zstd frame.
    Zstd {
        /// Byte offset of the magic number.
        offset: usize,
        /// Total frame length including header, blocks and checksum.
        length: usize,
        /// The parsed header.
        header: Box<FrameHeader>,
        /// One entry per block, in order.
        blocks: Vec<BlockHeader>,
    },
    /// A skippable frame — `dcz` dictionary transport uses one of these.
    Skippable {
        /// Byte offset of the magic number.
        offset: usize,
        /// Which of the sixteen skippable magics this is, 0..=15.
        variant: u8,
        /// Payload length, not counting the eight-byte prelude.
        payload: usize,
    },
}

/// What `walk` found before it stopped.
#[derive(Debug)]
pub struct Report {
    /// Frames in file order.
    pub frames: Vec<Frame>,
    /// Why the walk stopped short, if it did.
    pub trailing: Option<Error>,
    /// Bytes after the last frame the walk could not account for.
    pub trailing_bytes: usize,
}

/// Walk every frame in `data`, stopping at the first thing that is not one.
///
/// Deliberately permissive about the window: this is asking what a file *says*,
/// and refusing to report a frame's declared window because the number is
/// alarming would hide the very thing someone runs `info` to see.
pub fn walk(data: &[u8]) -> Report {
    let permissive = DecodeLimits {
        max_window_log: zalt_codec::params::WINDOWLOG_ABSOLUTE_MAX,
        max_decoded_size: None,
    };
    let mut frames = Vec::new();
    let mut at = 0usize;

    loop {
        if at == data.len() {
            return Report {
                frames,
                trailing: None,
                trailing_bytes: 0,
            };
        }
        match parse_one(data, at, &permissive) {
            Ok((frame, next)) => {
                frames.push(frame);
                at = next;
            }
            Err(e) => {
                return Report {
                    frames,
                    trailing: Some(e),
                    trailing_bytes: data.len() - at,
                };
            }
        }
    }
}

fn parse_one(data: &[u8], at: usize, limits: &DecodeLimits) -> Result<(Frame, usize), Error> {
    let rest = &data[at..];
    if rest.len() < 4 {
        return Err(Error::Truncated);
    }
    let magic = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]);

    match frame::classify(magic)? {
        FrameKind::Skippable => {
            let size_bytes = rest.get(4..8).ok_or(Error::Truncated)?;
            let payload =
                u32::from_le_bytes([size_bytes[0], size_bytes[1], size_bytes[2], size_bytes[3]])
                    as usize;
            let end = at
                .checked_add(8)
                .and_then(|n| n.checked_add(payload))
                .ok_or(Error::Truncated)?;
            if end > data.len() {
                return Err(Error::Truncated);
            }
            Ok((
                Frame::Skippable {
                    offset: at,
                    variant: (magic & 0x0F) as u8,
                    payload,
                },
                end,
            ))
        }
        FrameKind::Zstd => {
            let descriptor = *rest.get(4).ok_or(Error::Truncated)?;
            let need = frame::header_len_after_descriptor(descriptor)?;
            let tail = rest.get(5..5 + need).ok_or(Error::Truncated)?;
            let header = FrameHeader::parse(descriptor, tail, limits)?;

            let mut cursor = at + 5 + need;
            let mut blocks = Vec::new();
            loop {
                let bytes: [u8; BLOCK_HEADER_SIZE] = data
                    .get(cursor..cursor + BLOCK_HEADER_SIZE)
                    .ok_or(Error::Truncated)?
                    .try_into()
                    .map_err(|_| Error::Truncated)?;
                let block = BlockHeader::parse(bytes)?;
                cursor += BLOCK_HEADER_SIZE;
                cursor = cursor
                    .checked_add(block.stream_len() as usize)
                    .ok_or(Error::Truncated)?;
                if cursor > data.len() {
                    return Err(Error::Truncated);
                }
                let last = block.last;
                blocks.push(block);
                if last {
                    break;
                }
                // A frame with no last block would otherwise spin until the
                // input ran out; the truncation check above ends it, but only
                // after allocating a block record per three bytes of input.
                if blocks.len() > (u32::MAX as usize) / BLOCK_HEADER_SIZE {
                    return Err(Error::Malformed);
                }
            }

            if header.has_checksum {
                cursor = cursor.checked_add(4).ok_or(Error::Truncated)?;
                if cursor > data.len() {
                    return Err(Error::Truncated);
                }
            }

            Ok((
                Frame::Zstd {
                    offset: at,
                    length: cursor - at,
                    header: Box::new(header),
                    blocks,
                },
                cursor,
            ))
        }
    }
}

/// Render a report the way `zalt info` prints it.
pub fn render(name: &str, data: &[u8], report: &Report) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "{name}: {} ({} bytes)",
        human(data.len() as u64),
        data.len()
    );

    for frame in &report.frames {
        match frame {
            Frame::Skippable {
                offset,
                variant,
                payload,
            } => {
                let _ = writeln!(s, "\n  skippable frame @{offset}");
                let _ = writeln!(s, "    magic          0x184D2A5{variant:X}");
                let _ = writeln!(s, "    payload        {payload} bytes");
                if *variant == 0x0E {
                    // The one skippable variant with a meaning we know.
                    let _ = writeln!(s, "    note           dcz dictionary transport prelude");
                }
            }
            Frame::Zstd {
                offset,
                length,
                header,
                blocks,
            } => {
                let _ = writeln!(s, "\n  zstd frame @{offset}");
                let _ = writeln!(
                    s,
                    "    window         {} ({} bytes)",
                    human(header.window_size),
                    header.window_size
                );
                match header.frame_content_size {
                    Some(n) => {
                        let _ = writeln!(s, "    content size   {n} ({})", human(n));
                    }
                    None => {
                        let _ = writeln!(s, "    content size   undeclared");
                    }
                }
                let _ = writeln!(
                    s,
                    "    single segment {}",
                    if header.single_segment { "yes" } else { "no" }
                );
                match header.dictionary_id {
                    Some(id) => {
                        let _ = writeln!(s, "    dictionary     {id:#010x}");
                    }
                    None => {
                        let _ = writeln!(s, "    dictionary     none");
                    }
                }
                let _ = writeln!(
                    s,
                    "    checksum       {}",
                    if header.has_checksum { "yes" } else { "no" }
                );
                let _ = writeln!(s, "    block maximum  {}", human(header.block_size_max()));

                let (mut raw, mut rle, mut compressed) = (0usize, 0usize, 0usize);
                for b in blocks {
                    match b.block_type {
                        BlockType::Raw => raw += 1,
                        BlockType::Rle => rle += 1,
                        BlockType::Compressed => compressed += 1,
                    }
                }
                let _ = writeln!(
                    s,
                    "    blocks         {} ({compressed} compressed, {raw} raw, {rle} RLE)",
                    blocks.len()
                );

                // The per-block listing is the part worth having open next to
                // the RFC, but it is noise for a hundred-block frame.
                if blocks.len() <= 32 {
                    for (i, b) in blocks.iter().enumerate() {
                        let kind = match b.block_type {
                            BlockType::Raw => "raw",
                            BlockType::Rle => "rle",
                            BlockType::Compressed => "compressed",
                        };
                        let decoded = match b.decoded_len() {
                            Some(n) => format!(" -> {n}"),
                            None => String::new(),
                        };
                        let _ = writeln!(
                            s,
                            "      #{i:<3} {kind:<10} {:>8} bytes{decoded}{}",
                            b.stream_len(),
                            if b.last { "  (last)" } else { "" }
                        );
                    }
                }

                let _ = writeln!(s, "    frame bytes    {length}");
                if let Some(content) = header.frame_content_size
                    && *length > 0
                {
                    let _ = writeln!(
                        s,
                        "    ratio          {:.2}x",
                        content as f64 / *length as f64
                    );
                }
            }
        }
    }

    if let Some(e) = &report.trailing {
        let _ = writeln!(
            s,
            "\n  trailing       {} unparsed bytes: {e}",
            report.trailing_bytes
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `zstd -1` of "hello world", with a content checksum.
    const HELLO_ZST: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x24, 0x0b, 0x59, 0x00, 0x00, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x20,
        0x77, 0x6f, 0x72, 0x6c, 0x64, 0x68, 0x69, 0x1e, 0xb2,
    ];

    #[test]
    fn walks_a_real_frame() {
        let r = walk(HELLO_ZST);
        assert!(r.trailing.is_none(), "{:?}", r.trailing);
        assert_eq!(r.frames.len(), 1);

        let Frame::Zstd {
            header,
            blocks,
            length,
            offset,
        } = &r.frames[0]
        else {
            panic!("expected a zstd frame");
        };
        assert_eq!(*offset, 0);
        assert_eq!(header.frame_content_size, Some(11));
        assert!(header.has_checksum);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].last);
        assert_eq!(
            *length,
            HELLO_ZST.len(),
            "the walk must account for every byte, checksum included"
        );
    }

    /// Frames concatenate, and a decoder must handle that; so must the walk.
    #[test]
    fn walks_concatenated_frames() {
        let mut data = HELLO_ZST.to_vec();
        data.extend_from_slice(HELLO_ZST);
        let r = walk(&data);
        assert!(r.trailing.is_none());
        assert_eq!(r.frames.len(), 2);
    }

    #[test]
    fn reports_a_skippable_frame() {
        let mut data = Vec::new();
        data.extend_from_slice(&0x184D_2A5Eu32.to_le_bytes());
        data.extend_from_slice(&4u32.to_le_bytes());
        data.extend_from_slice(&[0xAA; 4]);
        data.extend_from_slice(HELLO_ZST);

        let r = walk(&data);
        assert!(r.trailing.is_none());
        assert_eq!(r.frames.len(), 2);
        assert!(matches!(
            r.frames[0],
            Frame::Skippable {
                variant: 0x0E,
                payload: 4,
                ..
            }
        ));
        assert!(render("x", &data, &r).contains("dcz dictionary transport"));
    }

    /// The walk must stop rather than run off the end, and say how much it
    /// could not account for.
    #[test]
    fn truncation_is_reported_not_panicked_on() {
        for cut in 1..HELLO_ZST.len() {
            let r = walk(&HELLO_ZST[..cut]);
            assert!(
                r.trailing.is_some(),
                "a {cut}-byte prefix must not parse as a whole frame"
            );
        }
    }

    /// A skippable frame declaring more payload than exists is a truncation,
    /// not an offset to jump to.
    #[test]
    fn an_oversized_skippable_payload_is_refused() {
        let mut data = Vec::new();
        data.extend_from_slice(&0x184D_2A50u32.to_le_bytes());
        data.extend_from_slice(&u32::MAX.to_le_bytes());
        let r = walk(&data);
        assert!(r.trailing.is_some());
        assert!(r.frames.is_empty());
    }

    #[test]
    fn a_non_frame_is_trailing_not_a_crash() {
        let r = walk(b"this is not a zstd file at all");
        assert!(r.frames.is_empty());
        assert!(matches!(r.trailing, Some(Error::BadMagic)));
        assert_eq!(r.trailing_bytes, 30);
    }

    #[test]
    fn rendering_a_real_frame_names_what_matters() {
        let r = walk(HELLO_ZST);
        let out = render("hello.zst", HELLO_ZST, &r);
        assert!(out.contains("content size   11"));
        assert!(out.contains("checksum       yes"));
        assert!(out.contains("dictionary     none"));
        assert!(out.contains("(last)"));
    }
}
