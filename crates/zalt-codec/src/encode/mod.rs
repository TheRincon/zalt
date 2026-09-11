//! The encoder (RFC 8878), correctness first.
//!
//! ## What this emits today
//!
//! Two strategies, both producing complete wire-valid frames:
//!
//! - [`Strategy::Stored`] — raw and RLE blocks, ratio 1.0. Not a placeholder:
//!   the `strategy = "stored"` path §7 specifies for already-compressed media,
//!   where zstd yields about 1.0x for the cost of a full encode.
//! - [`Strategy::Compress`] — compressed blocks with **raw literals and the
//!   predefined FSE tables**, so ratio comes entirely from matches. Measured at
//!   1.9x-7.2x on the corpora here, which is **1.3x-1.5x larger than
//!   `zstd -1`**. `just encode-ratio` reproduces that.
//!
//! ## Where the remaining gap is, and why the order was this way
//!
//! The two pieces not built are the two that close it: **Huffman-coded
//! literals**, which matter most because raw literals are stored a byte each
//! and literals dominate a typical block, and **fitted FSE tables**, where the
//! predefined distributions are replaced by ones counted from the block.
//! Neither changes whether a frame is legal, only how large it is — which is
//! precisely why they come second. §10: ratio work only after correctness.
//!
//! A block falls back to stored whenever the compressed form is not smaller,
//! so incompressible input costs headers and nothing else.
//!
//! ## One-shot, for now
//!
//! `compress` takes a whole input. The decoder is streaming because it must be
//! — it consumes hostile input off a socket — but nothing yet needs to *emit*
//! incrementally, and a streaming encoder is a state machine that should be
//! written when there is a caller to shape it.
//!
//! ## The frame writer is the parser's inverse, and is tested as such
//!
//! Every field here has a counterpart in [`crate::frame`] that has been read
//! against the reference corpus since M1. So the header is written to match
//! *that*, not to match a second reading of the RFC, and the property tests
//! assert the round trip rather than the byte layout: a header this module
//! writes must parse back to the parameters it was written from.

use alloc::vec::Vec;

use crate::error::{Error, Result};
use crate::params::{WINDOWLOG_ABSOLUTE_MAX, WINDOWLOG_MIN};

mod bitstream;
mod block;
mod frame;
mod fse;
mod huffman;
mod match_finder;
mod sequences;
mod split;

pub use block::{BLOCK_SIZE_DEFAULT, Emitted};
pub use frame::{MAX_HEADER, write_header};

/// Whether the encoder should attempt compression.
///
/// Mirrors the recipe-level notion in L3 without naming it: L1 takes a
/// parameter, and which content classes deserve which strategy is data that
/// lives a layer up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Strategy {
    /// Emit raw and RLE blocks only.
    ///
    /// The correct choice for content that is already compressed, and the only
    /// strategy this build implements.
    #[default]
    Stored,
    /// Find matches and emit compressed blocks.
    ///
    /// Raw literals and predefined FSE tables today; a block that would grow
    /// falls back to stored, so this is never worse than [`Strategy::Stored`]
    /// by more than the scan.
    Compress,
}

/// Parameters for one encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeParams {
    /// Compression level. Carried for the recipe plumbing, and not yet acted
    /// on: [`Strategy::Stored`] has no decisions a level could tune, and the
    /// match finder has no knobs to turn until there is more than one of it.
    pub level: i32,
    /// Whether to attempt compression at all.
    pub strategy: Strategy,
    /// `windowLog` to declare in the frame header.
    ///
    /// Must admit the block size: `Block_Maximum_Size` is
    /// `min(128 KiB, windowSize)`, so declaring a window smaller than the
    /// blocks emitted produces a frame a conformant decoder must refuse.
    /// [`EncodeParams::validate`] checks it rather than leaving it to be
    /// discovered by the other end.
    pub window_log: u8,
    /// Append the 4-byte content checksum.
    ///
    /// Off by default, matching the library's decode-side expectation and the
    /// traffic this codec targets. A checksum costs a full extra pass over the
    /// content, which §10 measured as a real fraction of decode time.
    pub checksum: bool,
    /// `Dictionary_ID` to declare, when the frame was compressed against one.
    ///
    /// Zero is not representable: the format spells "no dictionary" as an
    /// absent field or a zero value, and both mean the same thing, so `None`
    /// is the only way to say it.
    pub dictionary_id: Option<u32>,
    /// Largest block to emit, in bytes.
    pub block_size: u32,
}

impl Default for EncodeParams {
    fn default() -> Self {
        Self {
            level: 3,
            strategy: Strategy::Stored,
            // 128 KiB, which admits the largest block the format allows and so
            // never forces a split the content did not ask for.
            window_log: 17,
            checksum: false,
            dictionary_id: None,
            block_size: BLOCK_SIZE_DEFAULT,
        }
    }
}

impl EncodeParams {
    /// Check the parameters describe a frame a decoder will accept.
    ///
    /// Called before anything is written. A frame that is malformed by
    /// construction is worth refusing here, where the caller can see which
    /// parameter did it, rather than at whatever reads the bytes later.
    pub fn validate(&self) -> Result<()> {
        if !(WINDOWLOG_MIN..=WINDOWLOG_ABSOLUTE_MAX).contains(&self.window_log) {
            return Err(Error::WindowTooLarge {
                requested_log: self.window_log,
                limit_log: WINDOWLOG_ABSOLUTE_MAX,
            });
        }
        if self.block_size == 0 || self.block_size > BLOCK_SIZE_DEFAULT {
            return Err(Error::Malformed);
        }
        // The window must admit the blocks. Checked as u64: `1 << window_log`
        // reaches 2 GiB at the implementation maximum.
        if u64::from(self.block_size) > (1u64 << self.window_log) {
            return Err(Error::Malformed);
        }
        if self.dictionary_id == Some(0) {
            return Err(Error::Malformed);
        }
        Ok(())
    }
}

/// Compress `input` into a single zstd frame.
pub fn compress(input: &[u8], params: &EncodeParams) -> Result<Vec<u8>> {
    params.validate()?;

    let compress = params.strategy == Strategy::Compress;
    let block_size = params.block_size as usize;

    let mut out = Vec::with_capacity(input.len() + MAX_HEADER + 16);
    frame::write_header(&mut out, input.len() as u64, params);

    let header_len = out.len();
    block::write_all(
        &mut out,
        input,
        &split::fixed(input.len(), block_size),
        compress,
        params.window_log,
    );

    // Block splitting, §7 — attempted only where the content justifies it.
    //
    // `transitions` returns nothing unless the byte distribution actually
    // changes, which on measured corpora is every real file. So the common case
    // never reaches this branch and never pays to encode twice; content that
    // genuinely changes character partway through does, and keeps whichever
    // came out smaller. That comparison is what makes the feature unable to
    // regress: the fixed-stride blocks above are always the floor.
    if compress {
        let candidate = split::transitions(input, block_size);
        if !candidate.is_empty() {
            let mut alternative = Vec::with_capacity(out.len());
            block::write_all(
                &mut alternative,
                input,
                &candidate,
                compress,
                params.window_log,
            );
            if alternative.len() < out.len() - header_len {
                out.truncate(header_len);
                out.extend_from_slice(&alternative);
            }
        }
    }

    frame::write_epilogue(&mut out, input, params);
    Ok(out)
}

/// Compress with the default parameters.
pub fn compress_default(input: &[u8]) -> Result<Vec<u8>> {
    compress(input, &EncodeParams::default())
}
