//! # zalt-neon — L1a
//!
//! aarch64 NEON implementations of the [`Backend`] seam.
//!
//! ## aarch64 is not macOS
//!
//! This crate is gated on `target_arch = "aarch64"`, not on Apple. The kernels
//! are equally valid on aarch64 Linux. OS-specific integration (mach VM,
//! `madvise`, dispatch) belongs in `zalt-darwin`, which does not exist yet and
//! only will if profiling justifies it. Conflating the two would forfeit a
//! portability axis for nothing.
//!
//! ## No runtime dispatch
//!
//! NEON is architecturally mandatory on aarch64. There is no feature detection,
//! no dispatch table, and no `is_aarch64_feature_detected!`. Selection is a
//! compile-time type parameter.
//!
//! ## Safety
//!
//! This is the only crate in the workspace permitted to contain `unsafe`, and
//! every use is confined to the kernels below. `zalt-codec` is
//! `forbid(unsafe_code)` and stays that way.
//!
//! The kernels assume the [`Backend`] contract has been established by
//! `zalt_codec::lz`, which validates before dispatching. They are correct only
//! under that contract; see each function's safety note.

#![no_std]
#![deny(missing_docs)]

#[cfg(target_arch = "aarch64")]
mod kernels;

#[cfg(target_arch = "aarch64")]
pub use kernels::Neon;

/// The backend this build selects.
///
/// `Neon` on aarch64, [`zalt_codec::Scalar`] elsewhere. Exists so downstream
/// crates can name a backend without writing `cfg` themselves.
#[cfg(target_arch = "aarch64")]
pub type Selected = Neon;

/// The backend this build selects.
#[cfg(not(target_arch = "aarch64"))]
pub type Selected = zalt_codec::Scalar;
