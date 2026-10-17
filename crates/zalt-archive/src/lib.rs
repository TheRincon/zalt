//! # zalt-archive — ZSTA archives
//!
//! A multi-file archive built entirely from standard Zstandard frames, per the
//! ZSTA draft in `doc/ZSTA_SPEC.md` (format 0.2). An archive is at once:
//!
//! - **a valid `.tar.zst`** — `zstd -dc a.tar.zst | tar -x` extracts it, because
//!   every piece of metadata lives in skippable frames a stock decoder steps
//!   over, and the content frames decode to one POSIX pax tar stream; and
//! - **randomly accessible** — a trailing seek table and a Central Index list
//!   the contents without decoding anything, and extract any one entry by
//!   decoding only the frames that hold it, in parallel.
//!
//! ## Where this sits
//!
//! Above the codec, never inside it. `zalt-codec` writes and reads frames and
//! has never heard of tar, paths or archives; this crate arranges frames into
//! the ZSTA layout and is the only crate that knows the layout exists. The
//! layering rule from `doc/ZALT-MAC.md` §1 holds in the usual direction:
//! dependencies point down.
//!
//! ## What this milestone covers
//!
//! The *portable* profile (§1.1: stock-tool compatible, no dictionaries) and
//! an **R2** reader (§14.2): indexed open, listing, extracting one entry or
//! all of them, with §13's path, link and resource rules (R1); appending to a
//! finished archive (§10, [`append`](fn@append)); and rebuilding the trailer of one that
//! lost it (§11.3, [`recover`](fn@recover)). Also the macOS metadata of §9.1–§9.3 —
//! extended attributes (resource forks included), ACLs, BSD file flags, birth
//! time, owner names, FIFOs and device nodes. And entry hashes (§7.3): every
//! member's content hashed with BLAKE3 by default, or XXH3-128, and checked on
//! every read ([`hash`]). Not yet: dictionary mode.
//!
//! ## Features
//!
//! - **`native-fs`** (off by default): the [`fs`] module — archiving and
//!   extracting real files. It brings `cap-std` and `cap-fs-ext` for
//!   descriptor-relative extraction, `rustix` and `nix` for the system calls
//!   `std` lacks, and (macOS) `exacl` for ACLs, so this crate stays
//!   `forbid(unsafe_code)`. Everything else — the format, the writer, the
//!   reader, and the text forms of the metadata in [`meta`] — needs none of
//!   them and builds without it.
//!
//! One dependency is not optional: `blake3`, the BLAKE3 authors' crate, built
//! with its `pure` feature — no C, and no `unsafe` on aarch64 — for the
//! default entry hash.
//!
//! ## The evidence
//!
//! The spec defines conformance by what *other* software accepts (§14.1), so
//! the tests do too: every archive the writer produces is read back by
//! libzstd's reference seekable reader, `zstd(1)` and `tar`, through the
//! `zsta-seekable-probe` crate.

#![forbid(unsafe_code)]

pub mod append;
pub mod frames;
#[cfg(all(unix, feature = "native-fs"))]
pub mod fs;
pub mod hash;
pub mod index;
pub mod layout;
pub mod meta;
pub mod read;
pub mod recover;
pub mod tar;
pub mod varint;
pub mod write;
pub mod xxh3;

pub use append::append;
pub use hash::Hash;
pub use index::{Entry, Kind};
pub use read::{Archive, ReadLimits};
pub use recover::{RecoverOptions, Recovered, recover, recover_with};
pub use write::{FramePolicy, Writer, WriterOptions};

use core::fmt;

/// Everything that can go wrong reading or writing an archive.
#[derive(Debug)]
pub enum Error {
    /// The file is not a ZSTA archive, or not one this version reads. The
    /// string says which check failed.
    Format(String),
    /// The archive uses a feature this build does not implement.
    Unsupported(String),
    /// The Central Index disagrees with the tar stream, which §7.5 makes
    /// authoritative.
    Inconsistent(String),
    /// A §13 refusal: an unsafe path, a link escaping the root, a special file
    /// without opt-in, a limit exceeded.
    Refused(String),
    /// A frame failed to decode.
    Codec(zalt_codec::Error),
    /// An I/O error, with the path or stage it happened at.
    Io(String, std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Format(s) => write!(f, "not a valid ZSTA archive: {s}"),
            Self::Unsupported(s) => write!(f, "unsupported: {s}"),
            Self::Inconsistent(s) => write!(f, "index disagrees with the tar stream: {s}"),
            Self::Refused(s) => write!(f, "refused: {s}"),
            Self::Codec(e) => write!(f, "frame: {e}"),
            Self::Io(at, e) => write!(f, "{at}: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(_, e) => Some(e),
            _ => None,
        }
    }
}

impl From<zalt_codec::Error> for Error {
    fn from(e: zalt_codec::Error) -> Self {
        Self::Codec(e)
    }
}

/// Shorthand.
pub type Result<T> = core::result::Result<T, Error>;

pub(crate) fn format_err<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Format(msg.into()))
}
