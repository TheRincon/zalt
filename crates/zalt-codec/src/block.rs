//! Block headers (RFC 8878 §3.1.1.2).

use crate::error::{Error, Result};

/// Size of a block header on the wire.
pub const BLOCK_HEADER_SIZE: usize = 3;

/// What a block contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockType {
    /// Literal bytes, stored uncompressed.
    Raw,
    /// A single byte, repeated.
    Rle,
    /// A literals section followed by a sequences section.
    Compressed,
}

/// A parsed block header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockHeader {
    /// Whether this is the final block of the frame.
    pub last: bool,
    /// What the block contains.
    pub block_type: BlockType,
    /// For [`BlockType::Raw`] and [`BlockType::Compressed`], the number of
    /// bytes the block occupies in the stream.
    ///
    /// For [`BlockType::Rle`] this is instead the **decompressed** size: the
    /// block occupies exactly one byte in the stream regardless of how large
    /// this is. Conflating the two is how an RLE block turns into either a
    /// truncated read or an enormous one.
    pub size: u32,
}

impl BlockHeader {
    /// Parse the 3-byte little-endian block header.
    pub fn parse(bytes: [u8; BLOCK_HEADER_SIZE]) -> Result<Self> {
        let raw = u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16);

        let block_type = match (raw >> 1) & 0b11 {
            0 => BlockType::Raw,
            1 => BlockType::Rle,
            2 => BlockType::Compressed,
            // Block_Type 3 is Reserved. "This is not a valid block. This value
            // must not be produced by a compliant encoder."
            _ => return Err(Error::Reserved),
        };

        Ok(Self {
            last: raw & 1 != 0,
            block_type,
            size: raw >> 3,
        })
    }

    /// Bytes this block occupies in the input stream.
    pub fn stream_len(&self) -> u32 {
        match self.block_type {
            BlockType::Rle => 1,
            _ => self.size,
        }
    }

    /// Bytes this block contributes to the output, when that is known from the
    /// header alone.
    ///
    /// `None` for [`BlockType::Compressed`], whose output size is only known
    /// after decoding.
    pub fn decoded_len(&self) -> Option<u32> {
        match self.block_type {
            BlockType::Raw | BlockType::Rle => Some(self.size),
            BlockType::Compressed => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// From real `zstd -1` output on empty input: `01 00 00`.
    #[test]
    fn empty_last_raw_block() {
        let h = BlockHeader::parse([0x01, 0x00, 0x00]).unwrap();
        assert!(h.last);
        assert_eq!(h.block_type, BlockType::Raw);
        assert_eq!(h.size, 0);
    }

    /// From real `zstd -1` output on "hello world": `59 00 00`.
    #[test]
    fn eleven_byte_last_raw_block() {
        let h = BlockHeader::parse([0x59, 0x00, 0x00]).unwrap();
        assert!(h.last);
        assert_eq!(h.block_type, BlockType::Raw);
        assert_eq!(h.size, 11);
        assert_eq!(h.stream_len(), 11);
    }

    /// From real `zstd -1` output on 4096 random bytes: `01 80 00`.
    #[test]
    fn four_kib_last_raw_block() {
        let h = BlockHeader::parse([0x01, 0x80, 0x00]).unwrap();
        assert!(h.last);
        assert_eq!(h.block_type, BlockType::Raw);
        assert_eq!(h.size, 4096);
    }

    #[test]
    fn reserved_block_type_is_rejected() {
        // type bits = 3
        assert!(matches!(
            BlockHeader::parse([0b0000_0110, 0, 0]),
            Err(Error::Reserved)
        ));
    }

    /// An RLE block occupies one byte no matter how much it expands to. This
    /// asymmetry is the whole reason `stream_len` exists separately from `size`.
    #[test]
    fn rle_stream_length_is_one_byte() {
        // type = 1 (RLE), size = 100000, not last
        let raw = (100_000u32 << 3) | (1 << 1);
        let h = BlockHeader::parse([raw as u8, (raw >> 8) as u8, (raw >> 16) as u8]).unwrap();
        assert_eq!(h.block_type, BlockType::Rle);
        assert_eq!(h.size, 100_000);
        assert_eq!(h.stream_len(), 1, "RLE consumes one byte of input");
        assert_eq!(h.decoded_len(), Some(100_000));
    }

    #[test]
    fn compressed_output_size_is_unknown_from_the_header() {
        let raw = (500u32 << 3) | (2 << 1);
        let h = BlockHeader::parse([raw as u8, (raw >> 8) as u8, (raw >> 16) as u8]).unwrap();
        assert_eq!(h.block_type, BlockType::Compressed);
        assert_eq!(h.stream_len(), 500);
        assert_eq!(h.decoded_len(), None);
    }

    #[test]
    fn max_block_size_field_round_trips() {
        let raw = ((1u32 << 21) - 1) << 3;
        let h = BlockHeader::parse([raw as u8, (raw >> 8) as u8, (raw >> 16) as u8]).unwrap();
        assert_eq!(h.size, (1 << 21) - 1);
    }
}
