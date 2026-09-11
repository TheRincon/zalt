//! Writing raw and RLE blocks (RFC 8878 §3.1.1.2).
//!
//! `Block_Header` is three little-endian bytes: bit 0 `Last_Block`, bits 1-2
//! `Block_Type`, bits 3-23 `Block_Size`. For raw and RLE blocks `Block_Size` is
//! the size of the *decoded* content — for RLE that is the run length, from a
//! one-byte payload, which is the only place in the format where the header's
//! size field and the bytes on the wire disagree.

use alloc::vec::Vec;

use super::match_finder;
use super::sequences;
use crate::block::BlockType;

/// Largest block the format allows, and this encoder's default.
pub const BLOCK_SIZE_DEFAULT: u32 = 128 * 1024;

/// What a block turned out to be, for tests and diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emitted {
    /// Content copied verbatim.
    Raw,
    /// A run of one byte value, stored as that byte.
    Rle,
    /// Literals and sequences.
    Compressed,
}

/// Write one block header.
fn write_header(out: &mut Vec<u8>, last: bool, block_type: BlockType, size: u32) {
    debug_assert!(
        size <= BLOCK_SIZE_DEFAULT,
        "block size exceeds the format maximum"
    );
    let value = u32::from(last) | ((block_type as u32) << 1) | (size << 3);
    out.extend_from_slice(&value.to_le_bytes()[..3]);
}

/// Write one block, choosing the cheaper representation.
///
/// RLE is checked even under a "stored" strategy because it costs one scan with
/// an early exit on the second byte, and turns a block of padding from
/// `block_size` bytes into one. It is not compression in the sense
/// [`super::Strategy::Compress`] means — no entropy coding, no match finder —
/// so it is not a quiet fallback to the thing this build refuses to do.
pub fn write_one(out: &mut Vec<u8>, content: &[u8], last: bool) -> Emitted {
    let size = content.len() as u32;

    // An empty block cannot be RLE: the size field would say zero and the
    // payload byte would still be there, which is a byte of nothing.
    let run = !content.is_empty() && content.iter().all(|b| *b == content[0]);

    if run {
        write_header(out, last, BlockType::Rle, size);
        out.push(content[0]);
        Emitted::Rle
    } else {
        write_header(out, last, BlockType::Raw, size);
        out.extend_from_slice(content);
        Emitted::Raw
    }
}

/// Write one compressed block, falling back to stored if it does not pay.
///
/// The fallback is not an optimisation. A compressed block that is larger than
/// the content it encodes is legal but strictly worse, and on incompressible
/// input the literals section plus an empty sequences section is *always*
/// larger — so without this, "compress" would reliably expand exactly the data
/// `strategy = "stored"` exists to protect.
pub fn write_compressed(
    out: &mut Vec<u8>,
    matcher: &mut match_finder::Matcher,
    input: &[u8],
    start: usize,
    end: usize,
    last: bool,
) -> Emitted {
    let content = &input[start..end];
    let stored_len = stored_cost(content);

    // Snapshot first: parsing advances the repeat table, and a block that ends
    // up stored advances nothing on the decoder's side. Leaving ours ahead of
    // theirs would mistranslate every later repeat code — wrong bytes, not an
    // error.
    let saved = matcher.repeats();
    let parse = matcher.parse(input, start, end);
    let mut body = Vec::with_capacity(content.len() / 2 + 16);
    sequences::write_literals(&mut body, &parse.literals);
    sequences::write_sequences(&mut body, &parse.sequences);

    // `Block_Size` for a compressed block is its size *on the wire*, unlike raw
    // and RLE where it is the decoded size. It must also fit the 21-bit field,
    // which a body larger than the content never will in practice — but the
    // comparison below rules it out regardless.
    if body.len() >= stored_len || body.len() > BLOCK_SIZE_DEFAULT as usize {
        matcher.restore_repeats(saved);
        return write_one(out, content, last);
    }

    write_header(out, last, BlockType::Compressed, body.len() as u32);
    out.extend_from_slice(&body);
    Emitted::Compressed
}

/// Bytes a stored representation of this content would occupy, payload only.
fn stored_cost(content: &[u8]) -> usize {
    let run = !content.is_empty() && content.iter().all(|b| *b == content[0]);
    if run { 1 } else { content.len() }
}

/// Write every block for `input`, splitting at `block_size`.
///
/// Always emits at least one block, so an empty input becomes an empty final
/// raw block rather than a frame with no blocks in it — which is what the
/// format requires and what libzstd emits for empty input.
pub fn write_all(
    out: &mut Vec<u8>,
    input: &[u8],
    boundaries: &[usize],
    compress: bool,
    window_log: u8,
) -> Vec<Emitted> {
    let mut emitted = Vec::new();
    if input.is_empty() {
        emitted.push(write_one(out, &[], true));
        return emitted;
    }

    // One matcher for the whole frame, so its history, hash chains and repeat
    // table carry across block boundaries. A matcher per block would confine
    // every offset to its own block, which is what the first version did.
    let mut matcher = compress.then(|| match_finder::Matcher::new(window_log));

    let mut offset = 0usize;
    for &end in boundaries.iter().chain(core::iter::once(&input.len())) {
        if end <= offset {
            continue;
        }
        let last = end == input.len();
        emitted.push(match matcher.as_mut() {
            Some(m) => write_compressed(out, m, input, offset, end, last),
            None => write_one(out, &input[offset..end], last),
        });
        offset = end;
    }
    emitted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::BlockHeader;

    fn parse_first(bytes: &[u8]) -> BlockHeader {
        BlockHeader::parse([bytes[0], bytes[1], bytes[2]]).expect("a header we just wrote")
    }

    #[test]
    fn a_header_written_here_parses_back() {
        // The same inverse property the frame writer rests on, at block scale.
        for (last, size) in [(false, 0u32), (true, 1), (false, 12_345), (true, 131_072)] {
            let mut out = Vec::new();
            write_header(&mut out, last, BlockType::Raw, size);
            assert_eq!(out.len(), 3);
            let h = parse_first(&out);
            assert_eq!(h.last, last);
            assert_eq!(h.size, size);
            assert_eq!(h.block_type, BlockType::Raw);
        }
    }

    #[test]
    fn a_run_becomes_one_byte_and_the_header_carries_the_run_length() {
        let mut out = Vec::new();
        assert_eq!(write_one(&mut out, &[0x5a; 4096], true), Emitted::Rle);
        let h = parse_first(&out);
        assert_eq!(h.block_type, BlockType::Rle);
        // The header says 4096 decoded bytes; the wire carries one.
        assert_eq!(h.size, 4096);
        assert_eq!(out.len(), 4);
        assert_eq!(out[3], 0x5a);
    }

    #[test]
    fn anything_that_is_not_a_run_is_raw() {
        let mut out = Vec::new();
        let content = [1u8, 1, 1, 2];
        assert_eq!(write_one(&mut out, &content, false), Emitted::Raw);
        let h = parse_first(&out);
        assert_eq!(h.block_type, BlockType::Raw);
        assert_eq!(h.size, 4);
        assert_eq!(&out[3..], &content);
    }

    #[test]
    fn an_empty_block_is_raw_not_a_zero_length_run() {
        let mut out = Vec::new();
        assert_eq!(write_one(&mut out, &[], true), Emitted::Raw);
        assert_eq!(out.len(), 3, "no payload byte for an empty block");
        assert_eq!(parse_first(&out).size, 0);
    }

    #[test]
    fn splitting_marks_only_the_final_block_last() {
        let mut out = Vec::new();
        // Deliberately not a multiple: the last chunk is short, which is where
        // an off-by-one in the loop would show.
        let input: Vec<u8> = (0..250u32).map(|i| (i % 251) as u8).collect();
        let emitted = write_all(
            &mut out,
            &input,
            &crate::encode::split::fixed(input.len(), 100),
            false,
            17,
        );
        assert_eq!(emitted.len(), 3);

        let mut offset = 0usize;
        for (i, _) in emitted.iter().enumerate() {
            let h = parse_first(&out[offset..]);
            assert_eq!(h.last, i == 2, "block {i} last flag");
            assert_eq!(h.size, if i == 2 { 50 } else { 100 });
            offset += 3 + h.size as usize;
        }
        assert_eq!(offset, out.len(), "the blocks must tile the output exactly");
    }

    #[test]
    fn an_empty_input_still_produces_one_final_block() {
        let mut out = Vec::new();
        let emitted = write_all(&mut out, &[], &[], false, 17);
        assert_eq!(emitted.len(), 1);
        assert!(parse_first(&out).last);
        assert_eq!(parse_first(&out).size, 0);
    }
}
