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

use crate::backend::{Backend, Scalar};
use crate::dict::Dictionary;
use crate::error::{Error, Result};
use crate::params::{WINDOWLOG_ABSOLUTE_MAX, WINDOWLOG_MIN};
use crate::sequences::REP_INITIAL;
use match_finder::Prefix;

mod bitstream;
mod block;
mod frame;
mod fse;
mod huffman;
pub mod level;
mod match_finder;
mod optimal;
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
    /// Compression level, 1 to 19; 0 means the default (3). See [`level`] for
    /// what each one sets and why levels above 19 are treated as 19.
    ///
    /// Ignored by [`Strategy::Stored`], which has no decisions to tune.
    pub level: i32,
    /// Whether to attempt compression at all.
    pub strategy: Strategy,
    /// The largest `windowLog` the frame may declare — a **ceiling**, not a
    /// request.
    ///
    /// The window is decoder memory, so it is the receiver's to bound: this
    /// says how much history the other end has agreed to hold. The level then
    /// picks a window within it, and the frame declares no more than the
    /// content needs — a 10 KB document declares a 16 KB window whatever the
    /// level, because history that does not exist need not be held.
    ///
    /// The default is 23 (8 MiB): RFC 8878's interop floor and §6's untrusted
    /// decode ceiling, so a default frame is one every browser accepts.
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
            level: level::LEVEL_DEFAULT,
            strategy: Strategy::Stored,
            // 8 MiB, the interop floor every browser decodes. A ceiling: see
            // the field. It was 17 (128 KiB) before levels existed, which
            // capped every large input at a window a fifth the size of
            // `zstd -3`'s and was worth 30% of output size on its own.
            window_log: 23,
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

/// Compress `input` into a single zstd frame, measuring matches with the
/// portable [`Scalar`] backend.
///
/// The output is byte-identical whichever backend measures — a backend changes
/// how fast the answer arrives, never the answer — so this is the reference.
/// To encode with the build's own backend (NEON on aarch64), call
/// [`compress_with`], or `zalt_embed::compress`, which picks it.
pub fn compress(input: &[u8], params: &EncodeParams) -> Result<Vec<u8>> {
    compress_with::<Scalar>(input, params)
}

/// [`compress`], with matches measured by backend `B`.
pub fn compress_with<B: Backend>(input: &[u8], params: &EncodeParams) -> Result<Vec<u8>> {
    encode::<B>(input, Prefix::NONE, params)
}

/// Compress `input` into a single zstd frame against `dictionary`, with
/// backend `B`.
///
/// The frame declares the dictionary's `Dictionary_ID` — whatever
/// `params.dictionary_id` says, since a frame naming any other could not be
/// decoded — except for a raw-content dictionary, which has none: such a
/// frame decodes only where the reader is told which dictionary to use.
///
/// Matches reach into the dictionary's content exactly as far as RFC 8878 §5
/// lets a decoder follow them, and the frame starts from the dictionary's
/// repeat offsets, as the decoder will. Its pre-built entropy tables are not
/// used: every block describes its own, which is always legal. The output for
/// the same input and dictionary is the same on every backend.
pub fn compress_with_dictionary<B: Backend>(
    input: &[u8],
    dictionary: &Dictionary,
    params: &EncodeParams,
) -> Result<Vec<u8>> {
    let content = dictionary.content();
    // One buffer, dictionary first: a match into it is then an ordinary
    // backwards offset, the decoder's own view of a dictionary.
    let mut buffer = Vec::with_capacity(content.len() + input.len());
    buffer.extend_from_slice(content);
    buffer.extend_from_slice(input);
    let prefix = Prefix {
        len: content.len(),
        repeats: dictionary.entropy().map_or(REP_INITIAL, |e| e.repeats),
    };
    let id = dictionary.id();
    let params = EncodeParams {
        dictionary_id: (id != 0).then_some(id),
        ..*params
    };
    encode::<B>(&buffer, prefix, &params)
}

/// Encode the frame `buffer[prefix.len..]`, whose dictionary, if any, is
/// `buffer[..prefix.len]`.
fn encode<B: Backend>(buffer: &[u8], prefix: Prefix, params: &EncodeParams) -> Result<Vec<u8>> {
    params.validate()?;
    let input = &buffer[prefix.len..];

    let tuning = (params.strategy == Strategy::Compress).then(|| level::tuning(params.level));
    let compress = tuning.is_some();

    // The window the frame will actually declare — the smallest of the caller's
    // ceiling, the level's choice, and what the content can use. Only ever
    // *smaller* than what validate() accepted, and never smaller than a block
    // the content can produce, because the content clamp rounds up.
    let window_log = effective_window_log(params, tuning, input.len());
    let params = &EncodeParams {
        window_log,
        block_size: params.block_size.min(1u32 << window_log.min(31)),
        ..*params
    };
    let block_size = params.block_size as usize;

    let mut out = Vec::with_capacity(input.len() + MAX_HEADER + 16);
    frame::write_header(&mut out, input.len() as u64, params);

    // Block boundaries: §7's split points where the content changes
    // character, fixed strides otherwise — decided *before* encoding, and
    // encoded once.
    //
    // This used to encode with fixed strides, then — wherever the detector
    // proposed split points — encode the whole input again with them and keep
    // the smaller. That made the splitter unable to lose, and cost a second full
    // encode wherever it fired, which on real data is nearly everywhere: about
    // 2.1x the time at levels 5 and 9. Measured on `corpus/` and on a 38 MB tar
    // of mixed crates, at levels 5, 9 and 19, the second encode chose the
    // detector's boundaries *every time*; encoding once with them gives
    // byte-identical output. The guarantee given up is that splitting can never
    // come out larger than fixed strides — kept by measurement now, not by
    // construction (doc/ZALT-MAC.md §10, "The splitter's second encode").
    //
    // Never at the fast levels: the detector alone is ~10% of their time for
    // a few hundredths of a percent, and libzstd does not split there either.
    let fast = tuning.is_some_and(|t| {
        matches!(
            t.finder,
            level::Finder::Fast { .. } | level::Finder::DoubleFast { .. }
        )
    });
    let proposed = if compress && !fast {
        split::transitions(input, block_size)
    } else {
        Vec::new()
    };
    let mut boundaries = if proposed.is_empty() {
        split::fixed(input.len(), block_size)
    } else {
        proposed
    };
    // Found in the frame; placed in the buffer, after the dictionary.
    for b in &mut boundaries {
        *b += prefix.len;
    }
    block::write_all::<B>(
        &mut out,
        buffer,
        prefix,
        &boundaries,
        tuning,
        params.window_log,
    );

    frame::write_epilogue(&mut out, input, params);
    Ok(out)
}

/// The `windowLog` a frame declares.
///
/// The caller's ceiling, then the level's own window, then the content: a
/// window larger than the input asks the decoder to reserve history that
/// cannot exist. That last clamp is also what keeps a high level from costing a
/// small resource anything — libzstd makes the same choice, and keeps a separate
/// parameter table for inputs under 16 KB largely to express it.
///
/// Stored frames ignore the level and keep the ceiling, clamped to the content
/// the same way.
fn effective_window_log(params: &EncodeParams, tuning: Option<level::Tuning>, len: usize) -> u8 {
    // Smallest `k` with `2^k >= len`, so the content always fits the window.
    let content_log = (usize::BITS - len.saturating_sub(1).leading_zeros()) as u8;
    let wanted = tuning.map_or(params.window_log, |t| t.window_log.min(params.window_log));
    wanted.min(content_log.max(WINDOWLOG_MIN))
}

/// Compress with the default parameters.
pub fn compress_default(input: &[u8]) -> Result<Vec<u8>> {
    compress(input, &EncodeParams::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The window a frame declares is decoder memory, so it must respect three
    /// bounds at once: the caller's ceiling, the level's choice, and the
    /// content — and never fall below what the content needs to fit.
    ///
    /// The ceiling is the browser's promise (§6: 8 MiB untrusted), and a frame
    /// past it is one every browser refuses, so the claim "no level emits such
    /// a frame" is checked here across every level, including those that clamp,
    /// and every size class from empty to a gigabyte.
    #[test]
    fn the_declared_window_respects_the_ceiling_the_level_and_the_content() {
        let base = EncodeParams {
            strategy: Strategy::Compress,
            ..Default::default()
        };
        let sizes = [
            0usize,
            1,
            1023,
            1024,
            1025,
            16_000,
            131_072,
            131_073,
            1 << 20,
            9 << 20,
            1 << 30,
        ];
        for level in -3..=25 {
            let tuning = level::tuning(level);
            for len in sizes {
                let params = EncodeParams { level, ..base };
                let w = effective_window_log(&params, Some(tuning), len);
                assert!(w <= 23, "level {level}, {len} bytes: windowLog {w}");
                assert!(
                    w <= tuning.window_log,
                    "level {level}: above its own window"
                );
                assert!(w >= WINDOWLOG_MIN, "level {level}, {len} bytes: {w}");
                // Either the content fits, or a ceiling — the level's or the
                // caller's — is what stopped it.
                assert!(
                    (1usize << w) >= len || w == tuning.window_log.min(params.window_log),
                    "level {level}, {len} bytes: windowLog {w} neither fits nor is capped"
                );
                // And no larger than the content needs, past the minimum.
                if len > (1 << WINDOWLOG_MIN) {
                    assert!(
                        (1usize << (w - 1)) < len,
                        "level {level}, {len} bytes: {w} oversized"
                    );
                }
            }
        }

        // An explicit ceiling below the level's window wins.
        let capped = EncodeParams {
            window_log: 17,
            level: 19,
            ..base
        };
        assert_eq!(
            effective_window_log(&capped, Some(level::tuning(19)), 9 << 20),
            17
        );
        // Stored frames have no level, and keep the ceiling clamped to content.
        assert_eq!(effective_window_log(&base, None, 9 << 20), 23);
        assert_eq!(effective_window_log(&base, None, 5000), 13);
    }
}
