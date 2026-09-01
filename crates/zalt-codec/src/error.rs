//! Error taxonomy.
//!
//! Variants are deliberately specific. A browser needs to distinguish "this
//! frame is malformed" (fail the resource) from "this frame is well-formed but
//! demands more memory than policy allows" (a candidate for retrying the
//! request without `zstd` in `Accept-Encoding`). Collapsing both into a generic
//! `Invalid` would force that policy decision to be guesswork.

//! # Crossing a WASM boundary
//!
//! This type is destined for a WIT `variant`, so every payload here is a fixed
//! scalar and the type stays `Copy`. That is a deliberate constraint, not an
//! accident:
//!
//! - **No `&'static str`, no `String`, no borrowed data.** A string payload has
//!   to become an owned `string` at the boundary — a guest allocation the host
//!   must free — for a value that should cost nothing. [`Feature`] exists so
//!   "which feature is unimplemented" is an integer rather than a message.
//! - **Stable discriminants.** [`Error::code`] pins a numeric value per variant
//!   for the C FFI and for WIT. Those numbers are ABI: reorder the variants and
//!   old callers silently misread errors, so codes are assigned explicitly
//!   rather than derived from declaration order.
//! - **`#[non_exhaustive]` is a Rust-side courtesy only.** A WIT `variant` is
//!   closed, so adding a case is a breaking change to the component interface
//!   regardless of what Rust permits. New cases need an interface version bump.

use core::fmt;

/// Result alias for codec operations.
pub type Result<T> = core::result::Result<T, Error>;

/// A structurally valid format feature this build does not implement.
///
/// Fieldless so it crosses WIT as an `enum` — an integer — rather than as a
/// string requiring allocation on both sides of the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Feature {
    /// Compressed blocks: literals section plus sequences section.
    CompressedBlocks,
    /// Decoding against a dictionary.
    Dictionaries,
}

impl Feature {
    /// Stable numeric value for the FFI and WIT boundaries.
    pub const fn code(self) -> u32 {
        match self {
            Self::CompressedBlocks => 1,
            Self::Dictionaries => 2,
        }
    }
}

impl fmt::Display for Feature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CompressedBlocks => f.write_str("compressed blocks"),
            Self::Dictionaries => f.write_str("dictionaries"),
        }
    }
}

/// A decode or encode failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// Frame magic did not match, or a legacy (v0.1–v0.7) frame was seen.
    ///
    /// Legacy frames are rejected by design, not by omission.
    BadMagic,

    /// A reserved bit or reserved field was non-zero.
    ///
    /// Covers `Frame_Header_Descriptor` bit 3, `Block_Type` 3, and the reserved
    /// bits of `Symbol_Compression_Modes`.
    Reserved,

    /// The frame declares a window larger than policy permits.
    ///
    /// Distinct from [`Error::Malformed`]: the frame may be entirely valid. See
    /// `doc/ZALT-MAC.md` §6 — this is the header-parse-time rejection that
    /// bounds the remote memory-exhaustion primitive.
    WindowTooLarge {
        /// `windowLog` the frame asked for.
        requested_log: u8,
        /// `windowLog` ceiling in force.
        limit_log: u8,
    },

    /// A match offset pointed outside the available window or dictionary.
    OffsetTooLarge {
        /// The offset the sequence asked for.
        offset: u64,
        /// Bytes actually addressable (window + dictionary).
        available: u64,
    },

    /// Structurally invalid input: bad sizes, inconsistent lengths, malformed
    /// entropy tables.
    Malformed,

    /// A block referenced entropy tables from a previous block, but none exist.
    ///
    /// Treeless literals or `Repeat` FSE mode as the first block of a frame.
    /// Its own variant because it is a use-before-initialisation bug class,
    /// worth spotting distinctly in fuzz triage.
    NoPriorTable,

    /// Decoded output exceeded a declared or configured limit.
    TooLarge,

    /// Content checksum did not match.
    ChecksumMismatch,

    /// The frame requires a dictionary that has not been supplied.
    ///
    /// Its own variant because a browser may be able to act on it: for
    /// Compression Dictionary Transport the dictionary is fetchable, so this is
    /// a recoverable condition rather than a corrupt frame.
    DictionaryRequired {
        /// The dictionary the frame asked for.
        id: u32,
    },

    /// A structurally valid feature that this build does not decode yet.
    ///
    /// Distinct from [`Error::Malformed`] so that "we cannot" is never reported
    /// as "the input is bad" — the difference matters when triaging fuzz
    /// findings against libzstd.
    Unimplemented(Feature),

    /// Input ended mid-structure.
    ///
    /// In a streaming context this is usually "need more input" rather than a
    /// fatal condition; the streaming layer distinguishes them.
    Truncated,
}

impl Error {
    /// Stable numeric code for the C FFI and the WIT boundary.
    ///
    /// These values are **ABI**. They are written out explicitly rather than
    /// derived from declaration order precisely so that reordering the variants
    /// cannot silently change what a caller on the other side of a boundary
    /// reads. Never reuse a retired code.
    pub const fn code(self) -> u32 {
        match self {
            Self::BadMagic => 1,
            Self::Reserved => 2,
            Self::WindowTooLarge { .. } => 3,
            Self::OffsetTooLarge { .. } => 4,
            Self::Malformed => 5,
            Self::NoPriorTable => 6,
            Self::TooLarge => 7,
            Self::ChecksumMismatch => 8,
            Self::Truncated => 9,
            Self::DictionaryRequired { .. } => 10,
            Self::Unimplemented(_) => 11,
        }
    }

    /// Whether the caller might succeed by acting on the error rather than
    /// failing the resource.
    ///
    /// A browser can fetch a missing dictionary, or retry a request without
    /// `zstd` in `Accept-Encoding` when a frame demands more memory than policy
    /// allows. It cannot do anything about malformed bytes. Collapsing these
    /// into one category would force that decision to be guesswork at the call
    /// site — which is the reason the taxonomy is this granular.
    pub const fn is_recoverable(self) -> bool {
        matches!(
            self,
            Self::DictionaryRequired { .. } | Self::WindowTooLarge { .. } | Self::Unimplemented(_)
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic => f.write_str("not a modern zstd frame"),
            Self::Reserved => f.write_str("reserved bit set"),
            Self::WindowTooLarge {
                requested_log,
                limit_log,
            } => write!(
                f,
                "frame requests windowLog {requested_log}, limit is {limit_log}"
            ),
            Self::OffsetTooLarge { offset, available } => {
                write!(f, "match offset {offset} exceeds {available} available")
            }
            Self::Malformed => f.write_str("malformed input"),
            Self::NoPriorTable => f.write_str("reuse of entropy table that was never set"),
            Self::DictionaryRequired { id } => write!(f, "frame requires dictionary {id:#010x}"),
            Self::Unimplemented(what) => write!(f, "not implemented yet: {what}"),
            Self::TooLarge => f.write_str("decoded size exceeds limit"),
            Self::ChecksumMismatch => f.write_str("content checksum mismatch"),
            Self::Truncated => f.write_str("truncated input"),
        }
    }
}

impl core::error::Error for Error {}
