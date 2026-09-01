//! # zalt-embed — L2
//!
//! What a host process needs and a codec should not contain: streaming policy,
//! memory bounding, backend selection.
//!
//! ## Content-agnostic by construction
//!
//! This layer knows about memory budgets and hostile input. It does not know
//! what is being decoded. Browser *policy* (an 8 MiB window default) lives
//! here; browser *identity* does not. Content-class tuning is L3's job, carried
//! as data. CI greps this crate for content vocabulary.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod alloc;
pub mod budget;
pub mod decoder;
pub mod policy;
pub mod pool;

pub use alloc::{BufferSource, NoSource, Recycler, RecyclerConfig, SharedSource};
pub use budget::{Budget, Lease};
pub use decoder::{Decoder, Error, Result, decompress};
pub use policy::Policy;
pub use pool::{DecoderPool, PoolConfig, PooledDecoder};

/// The backend this build selected, chosen at compile time.
#[cfg(target_arch = "aarch64")]
pub type DefaultBackend = zalt_neon::Selected;

/// The backend this build selected, chosen at compile time.
#[cfg(not(target_arch = "aarch64"))]
pub type DefaultBackend = zalt_codec::Scalar;

/// Name of the selected backend, for diagnostics.
pub fn backend_name() -> &'static str {
    <DefaultBackend as zalt_codec::Backend>::NAME
}
