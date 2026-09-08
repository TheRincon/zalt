//! Loading recipes and their dictionaries from a filesystem.
//!
//! The `std` half of the crate, and the only part of it that touches the
//! outside world. A host with no filesystem — the browser baking a recipe in —
//! builds a [`Recipe`] as a Rust value and never compiles this module.

use std::fs;
use std::path::{Path, PathBuf};

use zalt_codec::dict::Dictionary;

use crate::error::{Error, ErrorKind, Result};
use crate::parse::parse;
use crate::recipe::{DictionarySource, Recipe};
use crate::route::{Entry, RecipeSet};

/// Largest dictionary this will read from disk.
///
/// A sanity bound, not a security one: a recipe directory is local
/// configuration, not wire input. It exists so that a `path` pointing at
/// something that is not a dictionary — a disk image, a core dump, the wrong
/// file entirely — fails immediately instead of after reading it all into
/// memory to discover the magic is wrong.
pub const MAX_DICTIONARY_BYTES: u64 = 64 * 1024 * 1024;

/// The extension a recipe manifest carries.
pub const MANIFEST_EXTENSION: &str = "toml";

/// Read and parse one manifest, resolving its dictionary.
pub fn load_recipe(path: impl AsRef<Path>) -> Result<Entry> {
    let path = path.as_ref();
    let text = fs::read_to_string(path).map_err(|_| Error::whole_file(ErrorKind::Io))?;
    let recipe = parse(&text)?;
    let dictionary = resolve_dictionary(&recipe, path)?;
    Ok(Entry { recipe, dictionary })
}

/// Read every manifest in a directory into one set.
///
/// Entries are sorted by filename before loading, so a directory with an
/// ambiguity in it fails the same way every time. Without that, the collision
/// [`RecipeSet::insert_entry`] reports would name whichever recipe the
/// filesystem enumerated first, and a bug that reproduces on one machine and
/// not another is most of a day.
///
/// Only `*.toml` is read. Dictionaries sit in the same directory, and a loader
/// that tried to parse `sample.dict` as a manifest would fail on a file doing
/// exactly what it should.
pub fn load_directory(dir: impl AsRef<Path>) -> Result<RecipeSet> {
    let dir = dir.as_ref();
    let listing = fs::read_dir(dir).map_err(|_| Error::whole_file(ErrorKind::Io))?;

    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in listing {
        let entry = entry.map_err(|_| Error::whole_file(ErrorKind::Io))?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == MANIFEST_EXTENSION) {
            paths.push(path);
        }
    }
    paths.sort();

    let mut set = RecipeSet::new();
    for path in paths {
        set.insert_entry(load_recipe(&path)?)?;
    }
    Ok(set)
}

/// Resolve a recipe's dictionary against the directory its manifest sits in.
fn resolve_dictionary(recipe: &Recipe, manifest: &Path) -> Result<Option<Dictionary>> {
    let Some(reference) = recipe.dictionary.as_ref() else {
        return Ok(None);
    };

    let bytes = match &reference.source {
        DictionarySource::ById => return Ok(None),
        DictionarySource::Inline(bytes) => bytes.clone(),
        DictionarySource::Path(relative) => {
            // Relative to the manifest, not to the process's working directory:
            // a recipe directory is meant to be copyable and to work from
            // wherever it lands, and a path that only resolves from where the
            // binary happened to be launched is neither.
            let path = manifest
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(relative);
            read_dictionary_file(&path)?
        }
    };

    let dictionary =
        Dictionary::parse(&bytes).map_err(|_| Error::whole_file(ErrorKind::BadDictionary))?;

    // The manifest *claims* an id; the dictionary carries one. A disagreement
    // does not produce a decode error later — it produces wrong bytes, because
    // a frame can only be decoded with the dictionary that compressed it (§7).
    // The mismatch is cheap to catch here and invisible everywhere after.
    if let Some(claimed) = reference.id {
        if dictionary.id() != claimed {
            return Err(Error::whole_file(ErrorKind::DictionaryIdMismatch));
        }
    }

    Ok(Some(dictionary))
}

fn read_dictionary_file(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::metadata(path).map_err(|_| Error::whole_file(ErrorKind::Io))?;
    if metadata.len() > MAX_DICTIONARY_BYTES {
        return Err(Error::whole_file(ErrorKind::BadDictionary));
    }
    fs::read(path).map_err(|_| Error::whole_file(ErrorKind::Io))
}
