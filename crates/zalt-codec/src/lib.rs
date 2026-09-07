//! # zalt-codec — L1
//!
//! A clean-room Zstandard (RFC 8878) implementation. **Modern frames only**:
//! the legacy v0.1–v0.7 formats are deliberately unsupported.
//!
//! ## Layering contract
//!
//! This crate knows about bytes and the zstd format. It knows nothing about
//! browsers, ZTML, Apple Silicon, or any content class. That is enforced in CI
//! by `ci/layering.sh`, which greps this crate for higher-layer vocabulary and
//! fails the build on a hit.
//!
//! If tuning for a content class ever seems to require a change in here, the
//! seam in a higher layer was too narrow. Widen it generically; do not add a
//! content-specific branch.
//!
//! ## Safety
//!
//! The whole crate is `forbid(unsafe_code)`. Every byte parsed by this crate is
//! attacker-controlled, so the parsing and table-construction paths carry no
//! unsafe at all. Acceleration that needs unsafe lives behind the [`backend`]
//! seam in a separate crate (`zalt-neon`), where it is a small, audited surface
//! rather than a property of the format code.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod backend;
pub mod bits;
pub mod block;
pub mod decode;
pub mod dict;
pub mod error;
pub mod frame;
pub mod fse;
pub mod huf;
pub mod literals;
pub mod lz;
pub mod magic;
pub mod params;
pub mod sequences;
pub mod window;
pub mod xxh64;

// Remaining in M1 (see doc/ZALT-MAC.md §10):
//   literals.rs   literals section
//   huf.rs        Huffman table build + 1/4-stream decode
//   fse.rs        FSE table build + decode
//   sequences.rs  sequence section, LL/ML/OF
//   execute.rs    sequence execution + window management
//   dict.rs       dictionary loading (format, not training) -- M4
//   encode/       M5

pub use backend::{Backend, Scalar};
pub use decode::{
    Buffers, Decoder, FnSink, Progress, ScalarDecoder, Sink, Status, decompress, decompress_with,
};
pub use error::{Error, Feature, Result};
pub use frame::FrameHeader;
