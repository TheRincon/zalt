//! # zalt-recipe — L3, the mechanism
//!
//! A **recipe** is a manifest plus an optional dictionary, consumed as data. It
//! selects a dictionary, proposes encoder parameters and decoder policy, and
//! routes a content type to a profile. It cannot alter the bitstream, so it can
//! never break correctness — only ratio and speed. See `doc/ZALT-MAC.md` §7.
//!
//! ## This crate is the mechanism, never an instance
//!
//! Nothing here knows what a content class is. `application/ztml` is a string
//! that arrives in a manifest and leaves in a routing decision; this crate never
//! spells it, and CI greps for that. A recipe *instance* lives in its own crate
//! (`zalt-ztml`) and is mostly data. If tuning a content class ever seems to
//! need a change in here, the seam was too narrow — widen it generically.
//!
//! ## Why this depends on L1 and not L2
//!
//! A recipe *describes* policy; it does not apply it. Depending on `zalt-embed`
//! would pull L2's architecture-specific dependency graph behind every consumer
//! of a manifest, to gain a struct this crate can name in plain integers. So a
//! [`Recipe`] carries numbers, and the host — which already owns a policy —
//! folds them in with [`Decode::narrow`].
//!
//! ## A recipe may narrow a host's limits. It may never widen them
//!
//! This is the load-bearing rule of the whole crate, and it is enforced in code
//! rather than documented and hoped for. Every ceiling folds by *minimum*: a
//! manifest asking for a 2 GiB window against an 8 MiB host cap yields 8 MiB —
//! not an error, and emphatically not 2 GiB.
//!
//! A manifest is configuration, and configuration travels. It arrives from a
//! directory a host scanned, from a file named on a command line, from whatever
//! shipped alongside a dictionary. If a recipe could raise a window ceiling,
//! then every one of those paths becomes a way to talk a host out of the bound
//! it set for untrusted input — which is the amplification `doc/ZALT-MAC.md` §6
//! exists to prevent, re-introduced one layer up and out of sight of the
//! decoder that checks it.
//!
//! ## Safety
//!
//! `forbid(unsafe_code)`, like L1. A manifest is far less hostile than a frame,
//! but "less hostile" is not a category this workspace has a second standard
//! for.

#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod error;
#[cfg(feature = "std")]
pub mod load;
#[cfg(feature = "parse")]
pub mod parse;
pub mod recipe;
pub mod route;

pub use error::{Error, ErrorKind, Result};
pub use recipe::{Decode, DictionaryRef, DictionarySource, Encode, Match, Recipe, Rule, Strategy};
pub use route::{Entry, RecipeSet};

#[cfg(feature = "parse")]
pub use parse::parse;
