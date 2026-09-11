//! Writing the frame header and epilogue (RFC 8878 §3.1.1).
//!
//! The inverse of [`crate::frame::FrameHeader::parse`], field for field, and
//! deliberately written against *that* rather than against a second reading of
//! the specification. The parser has been run over the reference corpus since
//! M1; a writer derived from it inherits that, where a writer derived from the
//! prose inherits only whichever misreadings the prose invites — and §4 records
//! one the prose already caused.

use alloc::vec::Vec;

use super::EncodeParams;
use crate::magic;
use crate::xxh64::Xxh64;

/// Largest header this writer emits: magic 4 + descriptor 1 + window 1 +
/// dictionary id 4 + content size 8.
pub const MAX_HEADER: usize = 18;

/// Content up to this size may be written as a single segment.
///
/// `Single_Segment_flag` drops the window descriptor byte and makes the frame
/// content size double as the window size — which obliges the decoder to hold
/// the entire content at once. That is a saving of one byte against a demand on
/// somebody else's memory, so it is taken only where the demand is trivially
/// small.
///
/// 128 KiB is the largest block the format allows, so below this the frame is
/// one block anyway and a decoder that could hold the block can hold the
/// content. Above it, a window descriptor is written and the decoder may
/// stream.
const SINGLE_SEGMENT_MAX: u64 = 128 * 1024;

/// `Frame_Content_Size_flag` and the field width it implies.
///
/// The flag's meaning depends on `Single_Segment`, which is the asymmetry the
/// parser documents and the one place this writer can silently disagree with
/// it:
///
/// - single segment: flag 0 is a **1-byte** field, so a small size costs one
///   byte and the size is always present (it has to be — it is the window).
/// - otherwise: flag 0 means **no field at all**. A size below 256 therefore
///   cannot be declared cheaply, because the 2-byte form stores `size - 256`
///   and cannot express it. It is omitted rather than paid for with four bytes;
///   the frame is legal either way and a decoder that wants to preallocate can
///   still read the block header.
fn content_size_field(size: u64, single_segment: bool) -> (u8, usize) {
    match size {
        0..=255 if single_segment => (0, 1),
        0..=255 => (0, 0),
        256..=65_791 => (1, 2),
        s if s <= u64::from(u32::MAX) => (2, 4),
        _ => (3, 8),
    }
}

/// `Window_Descriptor` byte describing a window of exactly `1 << log`.
///
/// `windowLog = 10 + Exponent`, and the mantissa adds eighths on top. Only the
/// exponent is used: a zero mantissa gives exactly `1 << log`, which is what
/// the parameters asked for, and spending the mantissa to declare a window
/// between two powers of two would save the decoder memory it has already
/// agreed to hold.
fn window_descriptor(log: u8) -> u8 {
    log.saturating_sub(10) << 3
}

/// Write the magic number and frame header.
pub fn write_header(out: &mut Vec<u8>, content_size: u64, params: &EncodeParams) {
    out.extend_from_slice(&magic::to_le_bytes(magic::ZSTD_FRAME));

    let single_segment = content_size <= SINGLE_SEGMENT_MAX
        && u64::from(params.block_size) >= content_size
        // A single-segment frame's window *is* its content size, so taking
        // this path must not declare a smaller window than the parameters did.
        && content_size <= (1u64 << params.window_log);

    let (fcs_flag, fcs_width) = content_size_field(content_size, single_segment);

    let did_flag: u8 = match params.dictionary_id {
        None => 0,
        Some(id) if id <= 0xFF => 1,
        Some(id) if id <= 0xFFFF => 2,
        Some(_) => 3,
    };

    let mut descriptor = fcs_flag << 6;
    if single_segment {
        descriptor |= 0b0010_0000;
    }
    if params.checksum {
        descriptor |= 0b0000_0100;
    }
    descriptor |= did_flag;
    out.push(descriptor);

    if !single_segment {
        out.push(window_descriptor(params.window_log));
    }

    if let Some(id) = params.dictionary_id {
        match did_flag {
            1 => out.push(id as u8),
            2 => out.extend_from_slice(&(id as u16).to_le_bytes()),
            _ => out.extend_from_slice(&id.to_le_bytes()),
        }
    }

    match fcs_width {
        0 => {}
        1 => out.push(content_size as u8),
        2 => out.extend_from_slice(&((content_size - 256) as u16).to_le_bytes()),
        4 => out.extend_from_slice(&(content_size as u32).to_le_bytes()),
        _ => out.extend_from_slice(&content_size.to_le_bytes()),
    }
}

/// Write whatever follows the last block.
///
/// The content checksum is the low 32 bits of XXH64 over the *decoded* content
/// — which is the input, since this encoder does not transform it.
pub fn write_epilogue(out: &mut Vec<u8>, content: &[u8], params: &EncodeParams) {
    if params.checksum {
        let mut h = Xxh64::new(0);
        h.update(content);
        out.extend_from_slice(&h.frame_checksum().to_le_bytes());
    }
}
