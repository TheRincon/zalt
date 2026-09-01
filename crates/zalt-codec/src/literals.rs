//! The literals section of a compressed block (RFC 8878 §3.1.1.3.1).

use alloc::vec::Vec;

use crate::error::{Error, Result};
use crate::huf;

/// How a literals section is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiteralsType {
    /// Stored uncompressed.
    Raw,
    /// A single byte, repeated.
    Rle,
    /// Huffman-coded, with a new tree described in this block.
    Compressed,
    /// Huffman-coded, reusing the **previous block's** tree.
    ///
    /// Stateful across blocks, and therefore a use-before-initialisation
    /// hazard: a treeless section in the first block of a frame has no table to
    /// reuse and must be rejected, not decoded against whatever is lying around.
    Treeless,
}

/// A parsed literals section header.
#[derive(Debug, Clone, Copy)]
pub struct Header {
    /// How the section is encoded.
    pub literals_type: LiteralsType,
    /// Decoded size of the literals.
    pub regenerated_size: u32,
    /// Encoded size, for the Huffman forms.
    pub compressed_size: u32,
    /// Whether the payload uses the four-stream layout.
    pub four_streams: bool,
    /// Bytes the header itself occupies.
    pub header_len: usize,
}

impl Header {
    /// Parse a literals section header.
    pub fn parse(input: &[u8]) -> Result<Self> {
        let b0 = *input.first().ok_or(Error::Truncated)?;
        let literals_type = match b0 & 0b11 {
            0 => LiteralsType::Raw,
            1 => LiteralsType::Rle,
            2 => LiteralsType::Compressed,
            _ => LiteralsType::Treeless,
        };
        let size_format = (b0 >> 2) & 0b11;

        match literals_type {
            LiteralsType::Raw | LiteralsType::Rle => {
                // Size_Format is 1 bit when even, 2 bits when odd — so `00` and
                // `10` mean the same thing here, and the second bit is part of
                // the size.
                let (header_len, regenerated_size) = match size_format {
                    0 | 2 => (1usize, u32::from(b0 >> 3)),
                    1 => {
                        let b1 = *input.get(1).ok_or(Error::Truncated)?;
                        (2, u32::from(b0 >> 4) | (u32::from(b1) << 4))
                    }
                    _ => {
                        let b1 = *input.get(1).ok_or(Error::Truncated)?;
                        let b2 = *input.get(2).ok_or(Error::Truncated)?;
                        (
                            3,
                            u32::from(b0 >> 4) | (u32::from(b1) << 4) | (u32::from(b2) << 12),
                        )
                    }
                };
                Ok(Self {
                    literals_type,
                    regenerated_size,
                    // RLE stores one byte; Raw stores the literals themselves.
                    compressed_size: match literals_type {
                        LiteralsType::Rle => 1,
                        _ => regenerated_size,
                    },
                    four_streams: false,
                    header_len,
                })
            }
            LiteralsType::Compressed | LiteralsType::Treeless => {
                let (header_len, regen_bits, four_streams) = match size_format {
                    0 => (3usize, 10u32, false),
                    1 => (3, 10, true),
                    2 => (4, 14, true),
                    _ => (5, 18, true),
                };
                let bytes = input.get(..header_len).ok_or(Error::Truncated)?;
                let mut value = 0u64;
                for (i, &b) in bytes.iter().enumerate() {
                    value |= u64::from(b) << (8 * i);
                }
                let mask = (1u64 << regen_bits) - 1;
                let regenerated_size = ((value >> 4) & mask) as u32;
                let compressed_size = ((value >> (4 + regen_bits)) & mask) as u32;
                Ok(Self {
                    literals_type,
                    regenerated_size,
                    compressed_size,
                    four_streams,
                    header_len,
                })
            }
        }
    }

    /// Total bytes this section occupies, header plus payload.
    pub fn total_len(&self) -> usize {
        self.header_len + self.compressed_size as usize
    }
}

/// Decode a literals section.
///
/// `previous` carries the Huffman table from an earlier block in the same
/// frame, for treeless sections. On success it is replaced when this section
/// describes a new tree.
///
/// `max_regenerated` is the block's `Block_Maximum_Size`. It is not a
/// convenience: `Regenerated_Size` is a 20-bit field, so a three-byte header
/// can claim 1,048,575 bytes — eight times the largest block the format
/// permits. Checking it here, before anything is allocated or decoded, is what
/// stops a four-byte RLE section from asking for a megabyte.
pub fn decode(
    input: &[u8],
    previous: &mut Option<huf::Table>,
    out: &mut Vec<u8>,
    max_regenerated: u64,
) -> Result<usize> {
    let header = Header::parse(input)?;
    if u64::from(header.regenerated_size) > max_regenerated {
        return Err(Error::TooLarge);
    }
    let regenerated = header.regenerated_size as usize;
    let payload = input
        .get(header.header_len..header.total_len())
        .ok_or(Error::Truncated)?;

    match header.literals_type {
        LiteralsType::Raw => {
            if payload.len() != regenerated {
                return Err(Error::Malformed);
            }
            out.extend_from_slice(payload);
        }
        LiteralsType::Rle => {
            let byte = *payload.first().ok_or(Error::Truncated)?;
            out.resize(out.len() + regenerated, byte);
        }
        LiteralsType::Compressed => {
            let (table, consumed) = huf::Table::parse(payload)?;
            let streams = payload.get(consumed..).ok_or(Error::Truncated)?;
            if header.four_streams {
                table.decode_four(streams, regenerated, out)?;
            } else {
                table.decode_single(streams, regenerated, out)?;
            }
            *previous = Some(table);
        }
        LiteralsType::Treeless => {
            let table = previous.as_ref().ok_or(Error::NoPriorTable)?;
            if header.four_streams {
                table.decode_four(payload, regenerated, out)?;
            } else {
                table.decode_single(payload, regenerated, out)?;
            }
        }
    }

    Ok(header.total_len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_one_byte_header() {
        // type 0, size_format 00, regenerated size 5.
        let h = Header::parse(&[5 << 3]).unwrap();
        assert_eq!(h.literals_type, LiteralsType::Raw);
        assert_eq!(h.regenerated_size, 5);
        assert_eq!(h.header_len, 1);
    }

    /// `00` and `10` are the same one-byte form: the second Size_Format bit is
    /// really the low bit of the size.
    #[test]
    fn raw_size_format_two_is_also_one_byte() {
        let h = Header::parse(&[(5 << 3) | 0b1000]).unwrap();
        assert_eq!(h.header_len, 1);
        assert_eq!(h.regenerated_size, 5);
    }

    #[test]
    fn raw_two_byte_header() {
        // type 0, size_format 01, 12-bit size.
        let size = 300u32;
        let b0 = ((size & 0xF) << 4) as u8 | 0b0100;
        let b1 = (size >> 4) as u8;
        let h = Header::parse(&[b0, b1]).unwrap();
        assert_eq!(h.regenerated_size, size);
        assert_eq!(h.header_len, 2);
    }

    #[test]
    fn raw_three_byte_header() {
        let size = 100_000u32;
        let b0 = ((size & 0xF) << 4) as u8 | 0b1100;
        let b1 = ((size >> 4) & 0xFF) as u8;
        let b2 = ((size >> 12) & 0xFF) as u8;
        let h = Header::parse(&[b0, b1, b2]).unwrap();
        assert_eq!(h.regenerated_size, size);
        assert_eq!(h.header_len, 3);
    }

    #[test]
    fn rle_payload_is_one_byte_regardless_of_size() {
        let h = Header::parse(&[(200 << 3) as u8 | 0b01]).unwrap();
        assert_eq!(h.literals_type, LiteralsType::Rle);
        assert_eq!(h.compressed_size, 1, "RLE stores exactly one byte");
    }

    /// Taken from real `zstd -3` output: header bytes `66 1d 4a` describe a
    /// four-stream compressed section, 470 bytes regenerated from 296.
    #[test]
    fn compressed_four_stream_header_from_real_zstd() {
        let h = Header::parse(&[0x66, 0x1d, 0x4a]).unwrap();
        assert_eq!(h.literals_type, LiteralsType::Compressed);
        assert!(h.four_streams);
        assert_eq!(h.regenerated_size, 470);
        assert_eq!(h.compressed_size, 296);
        assert_eq!(h.header_len, 3);
    }

    #[test]
    fn compressed_single_stream_header() {
        // size_format 00 -> one stream, 3-byte header.
        let regen = 500u32;
        let comp = 300u32;
        let value = (u64::from(regen) << 4) | (u64::from(comp) << 14) | 0b0010;
        let bytes = [value as u8, (value >> 8) as u8, (value >> 16) as u8];
        let h = Header::parse(&bytes).unwrap();
        assert!(!h.four_streams);
        assert_eq!(h.regenerated_size, regen);
        assert_eq!(h.compressed_size, comp);
    }

    #[test]
    fn compressed_five_byte_header_uses_18_bit_fields() {
        let regen = 200_000u32;
        let comp = 150_000u32;
        let value = (u64::from(regen) << 4) | (u64::from(comp) << 22) | 0b1110;
        let bytes = [
            value as u8,
            (value >> 8) as u8,
            (value >> 16) as u8,
            (value >> 24) as u8,
            (value >> 32) as u8,
        ];
        let h = Header::parse(&bytes).unwrap();
        assert_eq!(h.header_len, 5);
        assert_eq!(h.regenerated_size, regen);
        assert_eq!(h.compressed_size, comp);
    }

    #[test]
    fn treeless_without_a_prior_table_is_rejected() {
        // type 3 (treeless), size_format 00, 3-byte header.
        let value = (10u64 << 4) | (5u64 << 14) | 0b0011;
        let mut input = alloc::vec![value as u8, (value >> 8) as u8, (value >> 16) as u8];
        input.extend_from_slice(&[0u8; 5]);
        let mut previous = None;
        let mut out = Vec::new();
        assert_eq!(
            decode(&input, &mut previous, &mut out, 1 << 20).unwrap_err(),
            Error::NoPriorTable
        );
    }

    #[test]
    fn raw_section_round_trips() {
        let mut input = alloc::vec![4u8 << 3];
        input.extend_from_slice(b"abcd");
        let mut previous = None;
        let mut out = Vec::new();
        let consumed = decode(&input, &mut previous, &mut out, 1 << 20).unwrap();
        assert_eq!(out, b"abcd");
        assert_eq!(consumed, 5);
    }

    #[test]
    fn rle_section_expands() {
        let input = [(7u8 << 3) | 0b01, b'Z'];
        let mut previous = None;
        let mut out = Vec::new();
        let consumed = decode(&input, &mut previous, &mut out, 1 << 20).unwrap();
        assert_eq!(out, b"ZZZZZZZ");
        assert_eq!(consumed, 2);
    }

    #[test]
    fn truncated_headers_are_reported() {
        assert!(Header::parse(&[]).is_err());
        assert!(
            Header::parse(&[0b0100]).is_err(),
            "2-byte form, 1 byte given"
        );
        assert!(Header::parse(&[0b0010, 0x00]).is_err(), "3-byte form");
    }
}
