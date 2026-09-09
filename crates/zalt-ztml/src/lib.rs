//! # zalt-ztml — L3, the instance
//!
//! The first recipe. `doc/ZALT-MAC.md` §8 is the only section of that document
//! that discusses ZTML, and this is the only crate that may name it — every
//! layer below is content-blind by construction and CI greps them for it.
//!
//! ## Almost all of this crate is in `data/`
//!
//! That is the design working. §3: *"If it accumulates meaningful amounts of
//! Rust, that is a signal the seam in `zalt-recipe` is too narrow."* What is
//! here is a manifest, a dictionary, and the few lines needed to hand them to a
//! caller. `ci/layering.sh` counts the lines and complains if that stops being
//! true.
//!
//! ## Why the manifest exists twice
//!
//! [`MANIFEST`] is the `.toml` text; [`recipe`] builds the same thing as a Rust
//! value. A browser bakes the recipe into its binary and has no reason to carry
//! a TOML parser, so this crate depends on `zalt-recipe` with
//! `default-features = false` and cannot parse anything. The CLI, which loads
//! recipes from paths, gets the text.
//!
//! Two copies of one fact is a drift hazard, so a test parses [`MANIFEST`] and
//! asserts it equals [`recipe`]. Neither can move without the other.
//!
//! ## The dictionary
//!
//! [`DICTIONARY`] is a **content-prefix** dictionary: a concatenation of
//! representative documents, which §7 stages as v1 because it needs no
//! algorithm at all. Measured against the same corpus held out, it makes L9
//! output 23–36% smaller on documents of 3.4–11.8 KB — a 29.0% mean.
//! `zalt-recipe-train` builds it and `just ztml-sweep` is where the level came
//! from.
//!
//! It is a *raw-content* dictionary, so it carries no `Dictionary_ID`. That is
//! the exact shape RFC 9842 §5 specifies for `dcz`, which supplies the identity
//! out of band as a SHA-256 — see `zalt_recipe::dcz`, and the note in
//! `data/ztml.toml` for what the missing ID still costs everywhere else.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::vec;

use zalt_recipe::recipe::{
    Decode, DictionaryRef, DictionarySource, Encode, Match, Recipe, Rule, Strategy,
};

/// The manifest, as TOML text.
pub const MANIFEST: &str = include_str!("../data/ztml.toml");

/// The content-prefix dictionary.
pub const DICTIONARY: &[u8] = include_bytes!("../data/ztml-0.3.dict");

/// How the dictionary was built: inputs, order, and hashes.
pub const DICTIONARY_PROVENANCE: &str = include_str!("../data/ztml-0.3.provenance.txt");

/// This recipe's name.
pub const NAME: &str = "ztml-0.3";

/// The ZTML recipe, built without a parser.
///
/// Kept field-for-field identical to `data/ztml.toml`, which a test enforces.
pub fn recipe() -> Recipe {
    Recipe {
        name: NAME.into(),
        match_: Match {
            content_types: vec!["application/ztml".into()],
            extensions: vec![".zdoc".into()],
        },
        dictionary: Some(DictionaryRef {
            // No id: a raw content-prefix dictionary carries none.
            id: None,
            source: DictionarySource::Path("ztml-0.3.dict".into()),
        }),
        encode: Encode {
            level: Some(9),
            window_log: Some(17),
            strategy: Strategy::Compress,
        },
        decode: Decode {
            max_window_log: Some(23),
            max_decoded_size: None,
            total_memory: None,
        },
        rules: vec![Rule {
            match_: Match {
                content_types: vec![
                    "image/webp".into(),
                    "image/png".into(),
                    "image/avif".into(),
                    "image/jpeg".into(),
                    "video/av1".into(),
                    "font/woff2".into(),
                ],
                extensions: vec![
                    ".webp".into(),
                    ".png".into(),
                    ".avif".into(),
                    ".jpg".into(),
                    ".jpeg".into(),
                    ".woff2".into(),
                ],
            },
            strategy: Some(Strategy::Stored),
            level: None,
        }],
    }
}

/// The recipe with its dictionary already parsed, ready to route.
///
/// The dictionary is embedded, so this resolves without a filesystem — the
/// baked-in path from §7's three load routes.
pub fn entry() -> Result<zalt_recipe::Entry, zalt_codec::error::Error> {
    Ok(zalt_recipe::Entry {
        recipe: recipe(),
        dictionary: Some(zalt_codec::dict::Dictionary::parse(DICTIONARY)?),
    })
}
