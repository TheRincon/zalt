//! Dictionary loading (RFC 8878 §5).
//!
//! Loading only. **Training is not here and never will be**: selecting good
//! dictionary content is a search problem that belongs in a dev-time tool
//! (`zalt-recipe-train`), entirely off the decode path. The split is favourable
//! — §7 — because loading is a parser and training is an algorithm, and only
//! one of them has to be fast, small, and safe against hostile input.
//!
//! # Two kinds of dictionary
//!
//! A **raw content** dictionary is any buffer of at least 8 bytes with no
//! structure at all. It behaves exactly as the content half of a formatted one:
//! history in front of the frame, and nothing else.
//!
//! A **formatted** dictionary begins with [`magic::ZSTD_DICTIONARY`] and adds a
//! `Dictionary_ID` and pre-built entropy tables. The tables are the reason
//! dictionaries matter disproportionately to this decoder: §7 notes entropy
//! table construction is a *per-frame fixed cost*, and §10's profile measures it
//! at ~18% of a real `.zdoc` before a byte of content is decoded. A dictionary
//! carries those tables already built.
//!
//! The two are distinguished by the magic number, not by inspection. A raw
//! dictionary that happens to begin with those four bytes would be misread, and
//! the format offers no way around that — the reference has the same property.
//!
//! # What loading must validate
//!
//! A dictionary is as attacker-controlled as a frame whenever it arrives over
//! the wire (`dcz`), so every field is checked rather than trusted:
//!
//! - at least 8 bytes, or it is not a dictionary at all;
//! - `Dictionary_ID` is not zero, which the format reserves for "none";
//! - each entropy table parses within its accuracy and symbol limits — the FSE
//!   tables against the same limits a block's face, the Huffman table against
//!   [`huf::DICT_TABLELOG_MAX`], which is *one larger*. That divergence is not
//!   an oversight; see that constant;
//! - all three repeat offsets are non-zero and reach no further back than the
//!   content, since a repeat offset is used before any output exists and one
//!   pointing past the content would address memory the dictionary never had.
//!
//! # The window interaction, which is a trap
//!
//! RFC 8878 §5: sequences may reference the dictionary at offsets *larger than
//! the output so far*, and even larger than `Window_Size` — but only until
//! total output exceeds `Window_Size`, after which the dictionary is no longer
//! reachable. So "how far back may an offset reach" is not a constant during a
//! frame, and a decoder that fixes it at either bound is wrong in one direction
//! or the other. `lz::copy_match` already takes `dict_size` for this reason;
//! that parameter has been zero and its rejection path unreachable until now.

use alloc::vec::Vec;

use crate::error::{Error, Result};
use crate::fse::{self, DecodeTable, Distribution};
use crate::huf;
use crate::magic;

/// Smallest dictionary the format permits, formatted or raw.
pub const MIN_SIZE: usize = 8;

/// Repeat offsets a formatted dictionary carries.
pub const REPEAT_COUNT: usize = 3;

/// Bytes those repeat offsets occupy.
const REPEAT_BYTES: usize = REPEAT_COUNT * 4;

/// Entropy tables a formatted dictionary carries, pre-built.
///
/// These seed the decoder's cross-block state: the literals table becomes the
/// one a `Treeless` section reuses, and the three FSE tables become what
/// `Repeat` mode repeats. That is the whole performance argument for
/// dictionaries on small documents — the first block of the frame starts with
/// tables already built instead of building its own.
#[derive(Debug, Clone)]
pub struct Entropy {
    /// Huffman table for literals.
    pub literals: huf::Table,
    /// FSE table for offset codes.
    pub offsets: DecodeTable,
    /// FSE table for match lengths.
    pub match_lengths: DecodeTable,
    /// FSE table for literals lengths.
    pub literal_lengths: DecodeTable,
    /// Repeat offsets, replacing the usual `{1, 4, 8}`.
    pub repeats: [u32; REPEAT_COUNT],
}

/// A loaded dictionary.
#[derive(Debug, Clone)]
pub struct Dictionary {
    id: u32,
    content: Vec<u8>,
    entropy: Option<Entropy>,
}

impl Dictionary {
    /// Load a dictionary, formatted or raw content.
    pub fn parse(raw: &[u8]) -> Result<Self> {
        if raw.len() < MIN_SIZE {
            return Err(Error::Malformed);
        }

        let magic_bytes: [u8; 4] = raw[..4].try_into().map_err(|_| Error::Malformed)?;
        if u32::from_le_bytes(magic_bytes) != magic::ZSTD_DICTIONARY {
            // Raw content: no identity, no tables, just history.
            return Ok(Self {
                id: 0,
                content: raw.to_vec(),
                entropy: None,
            });
        }

        let id_bytes: [u8; 4] = raw[4..8].try_into().map_err(|_| Error::Malformed)?;
        let id = u32::from_le_bytes(id_bytes);
        if id == 0 {
            // Zero means "no dictionary", so a dictionary cannot claim it.
            return Err(Error::Malformed);
        }

        let mut rest = &raw[8..];

        // Order is fixed by the format: literals, offsets, match lengths,
        // literals lengths. Getting it wrong builds three valid tables and
        // assigns them to the wrong symbols — output that decodes and is wrong,
        // which is the failure mode this codec fears most.
        let (literals, used) = huf::Table::parse_dictionary(rest)?;
        rest = rest.get(used..).ok_or(Error::Truncated)?;

        let offsets = take_fse(&mut rest, fse::OF_ACCURACY_MAX, fse::OF_SYMBOL_MAX)?;
        let match_lengths = take_fse(&mut rest, fse::ML_ACCURACY_MAX, fse::ML_SYMBOL_MAX)?;
        let literal_lengths = take_fse(&mut rest, fse::LL_ACCURACY_MAX, fse::LL_SYMBOL_MAX)?;

        let repeat_bytes = rest.get(..REPEAT_BYTES).ok_or(Error::Truncated)?;
        rest = &rest[REPEAT_BYTES..];

        let mut repeats = [0u32; REPEAT_COUNT];
        for (slot, chunk) in repeats.iter_mut().zip(repeat_bytes.as_chunks::<4>().0) {
            *slot = u32::from_le_bytes(*chunk);
        }

        let content = rest.to_vec();

        // A repeat offset is usable before this frame has produced anything, so
        // it addresses the dictionary content and nothing else. Zero is never a
        // legal offset, and one reaching past the content addresses memory the
        // dictionary does not have.
        for rep in repeats {
            if rep == 0 || rep as usize > content.len() {
                return Err(Error::OffsetTooLarge {
                    offset: u64::from(rep),
                    available: content.len() as u64,
                });
            }
        }

        Ok(Self {
            id,
            content,
            entropy: Some(Entropy {
                literals,
                offsets,
                match_lengths,
                literal_lengths,
                repeats,
            }),
        })
    }

    /// `Dictionary_ID`, or zero for a raw-content dictionary.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// The content, which acts as history in front of the frame.
    pub fn content(&self) -> &[u8] {
        &self.content
    }

    /// Pre-built entropy tables, absent for a raw-content dictionary.
    pub fn entropy(&self) -> Option<&Entropy> {
        self.entropy.as_ref()
    }

    /// Whether this dictionary carries pre-built tables.
    pub fn has_entropy(&self) -> bool {
        self.entropy.is_some()
    }
}

/// Parse one FSE table description and advance the cursor past it.
fn take_fse(rest: &mut &[u8], max_accuracy: u32, max_symbol: u16) -> Result<DecodeTable> {
    let (distribution, used) = Distribution::parse(rest, max_accuracy, max_symbol)?;
    *rest = rest.get(used..).ok_or(Error::Truncated)?;
    DecodeTable::build(&distribution)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_content_needs_no_structure() {
        let raw = [0xAAu8; 64];
        let d = Dictionary::parse(&raw).unwrap();
        assert_eq!(d.id(), 0, "a raw dictionary has no identity");
        assert_eq!(d.content(), &raw[..]);
        assert!(!d.has_entropy());
    }

    #[test]
    fn anything_under_eight_bytes_is_not_a_dictionary() {
        for len in 0..MIN_SIZE {
            assert!(
                Dictionary::parse(&[0u8; MIN_SIZE][..len]).is_err(),
                "{len} bytes was accepted"
            );
        }
        assert!(Dictionary::parse(&[0u8; MIN_SIZE]).is_ok());
    }

    /// The magic decides, not the shape of what follows.
    #[test]
    fn raw_content_may_look_like_anything_except_the_magic() {
        let mut raw = [0u8; 32];
        raw[..4].copy_from_slice(&magic::to_le_bytes(magic::ZSTD_FRAME));
        let d = Dictionary::parse(&raw).unwrap();
        assert_eq!(d.id(), 0);
        assert!(!d.has_entropy(), "a frame magic is still raw content here");
    }

    #[test]
    fn a_formatted_dictionary_may_not_claim_id_zero() {
        let mut raw = [0u8; 32];
        raw[..4].copy_from_slice(&magic::to_le_bytes(magic::ZSTD_DICTIONARY));
        // id stays zero
        assert!(Dictionary::parse(&raw).is_err());
    }

    /// Truncation at every length must be an error, never a panic.
    #[test]
    fn truncating_a_formatted_dictionary_is_an_error_at_every_cut() {
        let mut raw = [0u8; 128];
        raw[..4].copy_from_slice(&magic::to_le_bytes(magic::ZSTD_DICTIONARY));
        raw[4..8].copy_from_slice(&1u32.to_le_bytes());
        for cut in MIN_SIZE..raw.len() {
            let _ = Dictionary::parse(&raw[..cut]);
        }
    }
}
