//! Dictionaries (§6): what a Dictionary frame may hold, and the set an
//! archive carries.
//!
//! A dictionary trades stock-tool compatibility for ratio on many small,
//! similar files. Every data frame written against one names it by
//! `Dictionary_ID`, so `zstd -dc` alone refuses the archive — but `zstd -D
//! dict -dc archive | tar -x` still extracts it, given the dictionary's bytes,
//! which are exactly a Dictionary frame's `User_Data` (`zalt archive dict`).
//!
//! §6 allows only the *formatted* kind — magic `0xEC30A437`, a non-zero ID,
//! entropy tables — because a frame names its dictionary by that ID, and raw
//! content has none to name.

use crate::{Error, Result, format_err};

/// One dictionary, as a Dictionary frame holds it: the bytes, and those bytes
/// loaded for the codec.
#[derive(Clone)]
pub struct Dictionary {
    raw: Vec<u8>,
    loaded: zalt_codec::dict::Dictionary,
}

impl Dictionary {
    /// A dictionary §6 allows: formatted, with a non-zero `Dictionary_ID`
    /// and tables that load. Raw content is refused, here and in an archive.
    pub fn parse(raw: Vec<u8>) -> Result<Self> {
        let magic = raw.get(..4).map(crate::layout::le32);
        if magic != Some(zalt_codec::magic::ZSTD_DICTIONARY) {
            return format_err(
                "a dictionary must be a formatted zstd dictionary, beginning 0xEC30A437 \
                 (§6: raw content has no ID for a frame to name)",
            );
        }
        let loaded = zalt_codec::dict::Dictionary::parse(&raw)
            .map_err(|e| Error::Format(format!("the dictionary does not load: {e}")))?;
        if loaded.id() == 0 || !loaded.has_entropy() {
            return format_err("a dictionary needs a non-zero Dictionary_ID (§6)");
        }
        Ok(Self { raw, loaded })
    }

    /// Its `Dictionary_ID`, never zero.
    pub fn id(&self) -> u32 {
        self.loaded.id()
    }

    /// The bytes a Dictionary frame holds — the same file `zstd -D` reads.
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    pub(crate) fn loaded(&self) -> &zalt_codec::dict::Dictionary {
        &self.loaded
    }
}

/// The same bytes are the same dictionary.
impl PartialEq for Dictionary {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl Eq for Dictionary {}

impl std::fmt::Debug for Dictionary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dictionary")
            .field("id", &format_args!("{:#010x}", self.id()))
            .field("bytes", &self.raw.len())
            .finish()
    }
}

/// The dictionary `id` names among `set`.
pub(crate) fn find(set: &[Dictionary], id: u32) -> Option<&Dictionary> {
    set.iter().find(|d| d.id() == id)
}

/// Check a set an archive carries: IDs unique (§6), total size bounded.
pub(crate) fn check_set(set: &[Dictionary], max_bytes: u64) -> Result<()> {
    let total: u64 = set.iter().map(|d| d.raw.len() as u64).sum();
    if total > max_bytes {
        return Err(Error::Refused(format!(
            "the archive's dictionaries hold {total} bytes, over the limit of {max_bytes}"
        )));
    }
    for (i, d) in set.iter().enumerate() {
        if set[..i].iter().any(|e| e.id() == d.id()) {
            return format_err(format!(
                "two dictionaries share Dictionary_ID {:#010x} (§6)",
                d.id()
            ));
        }
    }
    Ok(())
}
