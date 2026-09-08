//! The recipe itself: plain data, constructible without a parser.
//!
//! Every type here is an ordinary Rust value with public fields. That is the
//! point rather than an oversight — `doc/ZALT-MAC.md` §7 requires three load
//! paths (a directory, an embedded blob compiled into the binary, an ad-hoc
//! file), and a struct a host can simply *write* covers the middle one with no
//! parser in the binary at all. The text parser in [`crate::parse`] is one
//! producer of these values, not their definition.

use alloc::string::String;
use alloc::vec::Vec;

use zalt_codec::params::{DecodeLimits, WINDOWLOG_ABSOLUTE_MAX, WINDOWLOG_MIN};

/// Whether the encoder should bother.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Strategy {
    /// Compress normally.
    #[default]
    Compress,
    /// Emit stored blocks without attempting to compress.
    ///
    /// For content that is already compressed — WebP, WOFF2, AVIF — where zstd
    /// yields about 1.0x while costing a full encode. See §8.
    Stored,
}

/// Where a dictionary's bytes come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DictionarySource {
    /// A path, resolved relative to the manifest's own directory.
    ///
    /// Relative to the *manifest*, not to the process's working directory: a
    /// recipe directory is meant to be copyable, and a path that only resolves
    /// from wherever the binary happened to be launched is not.
    Path(String),
    /// Bytes carried directly, for a recipe compiled into a binary.
    Inline(Vec<u8>),
    /// Nothing local: both peers resolve the `id` out of band.
    ///
    /// The well-known-`Dictionary_ID` mechanism of §7, and where a `dcz`
    /// prelude's hash will eventually resolve to.
    ById,
}

/// Which dictionary a recipe selects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictionaryRef {
    /// The `Dictionary_ID` the manifest claims, if it names one.
    ///
    /// Claims, not defines. When bytes are loaded, this is **checked** against
    /// the dictionary's own id and a disagreement refuses the load — see
    /// [`crate::load`]. §7's caveat is that a frame can only be decoded with
    /// the dictionary that compressed it, so a mismatch here does not produce
    /// an error later; it produces wrong bytes.
    pub id: Option<u32>,
    /// Where the bytes come from.
    pub source: DictionarySource,
}

/// What content a recipe claims.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Match {
    /// Content types, normalised: lowercase, parameters stripped.
    pub content_types: Vec<String>,
    /// Filename extensions, normalised: lowercase, leading dot.
    pub extensions: Vec<String>,
}

impl Match {
    /// Whether this claims `content_type`, which need not be normalised.
    pub fn claims_content_type(&self, content_type: &str) -> bool {
        let wanted = normalise_content_type(content_type);
        self.content_types.contains(&wanted)
    }

    /// Whether this claims `filename`'s extension.
    pub fn claims_filename(&self, filename: &str) -> bool {
        let Some(ext) = extension_of(filename) else {
            return false;
        };
        self.extensions.contains(&ext)
    }

    /// Whether nothing at all is claimed.
    pub fn is_empty(&self) -> bool {
        self.content_types.is_empty() && self.extensions.is_empty()
    }
}

/// Encoder parameters a recipe proposes.
///
/// All optional: absent means "the encoder's own default", which is not the
/// same as any particular number this crate could pick. M5 consumes these; for
/// now they are carried and range-checked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Encode {
    /// Compression level.
    pub level: Option<i32>,
    /// `windowLog` to encode with.
    pub window_log: Option<u8>,
    /// Whether to attempt compression at all.
    pub strategy: Strategy,
}

/// Smallest compression level libzstd accepts, the `--fast=N` end.
pub const LEVEL_MIN: i32 = -131_072;

/// Largest compression level libzstd accepts.
pub const LEVEL_MAX: i32 = 22;

/// Decoder policy a recipe proposes.
///
/// **Proposes.** Nothing here is applied on its own; a host folds it into the
/// policy it already holds with [`Decode::narrow`], and the fold can only make
/// that policy stricter. See the crate documentation for why that direction is
/// the whole point.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Decode {
    /// Window ceiling as a `windowLog`.
    pub max_window_log: Option<u8>,
    /// Ceiling on a single frame's decoded output.
    pub max_decoded_size: Option<u64>,
    /// Ceiling on total live decoder memory.
    pub total_memory: Option<u64>,
}

impl Decode {
    /// Fold this recipe's limits into a host's, narrowing only.
    ///
    /// Every field takes the minimum of the two. A recipe that asks for more
    /// than the host allows gets the host's number back, silently and by
    /// design: refusing the whole load would turn a recipe into a way to *deny*
    /// service to a host whose policy is merely stricter than the recipe
    /// expected, and honouring it would turn a recipe into a way to lift a
    /// ceiling set for untrusted input.
    pub fn narrow(&self, host: DecodeLimits) -> DecodeLimits {
        DecodeLimits {
            max_window_log: match self.max_window_log {
                Some(mine) => mine.min(host.max_window_log),
                None => host.max_window_log,
            },
            max_decoded_size: match (self.max_decoded_size, host.max_decoded_size) {
                (Some(mine), Some(theirs)) => Some(mine.min(theirs)),
                // One bound where there was none is still a narrowing, whichever
                // side it came from.
                (Some(mine), None) => Some(mine),
                (None, theirs) => theirs,
            },
        }
    }

    /// Fold this recipe's total-memory ceiling into a host's, narrowing only.
    ///
    /// Separate from [`Decode::narrow`] because the total budget is L2's
    /// concept and `DecodeLimits` is L1's. Keeping them apart is what lets this
    /// crate stop at L1.
    pub fn narrow_total_memory(&self, host: u64) -> u64 {
        match self.total_memory {
            Some(mine) => mine.min(host),
            None => host,
        }
    }
}

/// An override that fires for particular content within a recipe.
///
/// §7's `[[rule]]`: the recipe covers a content class, and a rule carves out
/// the part of it that should be treated differently — most usefully the
/// already-compressed media that should not be compressed again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rule {
    /// What this rule fires on.
    pub match_: Match,
    /// Strategy override.
    pub strategy: Option<Strategy>,
    /// Level override.
    pub level: Option<i32>,
}

/// A manifest plus an optional dictionary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    /// Identifier, for diagnostics and for selecting one by name.
    pub name: String,
    /// What content this recipe claims.
    pub match_: Match,
    /// The dictionary it selects, if any.
    pub dictionary: Option<DictionaryRef>,
    /// Encoder parameters.
    pub encode: Encode,
    /// Decoder policy.
    pub decode: Decode,
    /// Overrides, tried in declaration order.
    pub rules: Vec<Rule>,
}

impl Recipe {
    /// An empty recipe with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            match_: Match::default(),
            dictionary: None,
            encode: Encode::default(),
            decode: Decode::default(),
            rules: Vec::new(),
        }
    }

    /// Encoder parameters for this content type, with any matching rule applied.
    ///
    /// The first rule that claims the content type wins. Declaration order is
    /// the tie-break so that a manifest reads top to bottom the way its author
    /// wrote it, rather than by some specificity heuristic they would have to
    /// learn.
    pub fn encode_for(&self, content_type: Option<&str>, filename: Option<&str>) -> Encode {
        let mut encode = self.encode;
        for rule in &self.rules {
            if !rule_claims(&rule.match_, content_type, filename) {
                continue;
            }
            if let Some(strategy) = rule.strategy {
                encode.strategy = strategy;
            }
            if let Some(level) = rule.level {
                encode.level = Some(level);
            }
            break;
        }
        encode
    }

    /// Whether this recipe claims the given content type or filename.
    pub fn claims(&self, content_type: Option<&str>, filename: Option<&str>) -> bool {
        rule_claims(&self.match_, content_type, filename)
    }
}

fn rule_claims(m: &Match, content_type: Option<&str>, filename: Option<&str>) -> bool {
    // Written without let-chains on purpose: the workspace declares
    // `rust-version = "1.85"` and let-chains landed in 1.88, so using one here
    // would break the stated MSRV for a line of syntax.
    if content_type.is_some_and(|ct| m.claims_content_type(ct)) {
        return true;
    }
    filename.is_some_and(|name| m.claims_filename(name))
}

/// Normalise a content type for comparison.
///
/// Lowercased, with parameters stripped: `text/html; charset=utf-8` and
/// `Text/HTML` both become `text/html`. A recipe that routes on `text/html` and
/// then does not fire because the origin sent a charset would be a bug reported
/// as "recipes do not work".
pub fn normalise_content_type(raw: &str) -> String {
    let base = raw.split(';').next().unwrap_or(raw);
    base.trim().to_ascii_lowercase()
}

/// Normalise a filename extension for comparison: lowercase, leading dot.
pub fn normalise_extension(raw: &str) -> String {
    let trimmed = raw.trim();
    let stripped = trimmed.strip_prefix('.').unwrap_or(trimmed);
    let mut out = String::with_capacity(stripped.len() + 1);
    out.push('.');
    out.push_str(&stripped.to_ascii_lowercase());
    out
}

/// The normalised extension of a filename, if it has one.
///
/// Takes the last dot, and only within the last path component, so
/// `archive.tar/file` has no extension and `/etc/rc.d/thing` is not `.d/thing`.
/// A leading dot is a hidden file rather than an extension: `.zdoc` alone is
/// named `.zdoc`, not an extensionless file of type zdoc.
pub fn extension_of(filename: &str) -> Option<String> {
    let base = filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(filename)
        .trim();
    let dot = base.rfind('.')?;
    if dot == 0 || dot + 1 == base.len() {
        return None;
    }
    Some(normalise_extension(&base[dot..]))
}

/// Convert a window size in bytes to the largest `windowLog` that fits inside it.
///
/// Rounds **down**, which is the conservative direction for a ceiling: a
/// manifest asking for a 10 MiB cap gets 8 MiB, never 16. Returns `None` below
/// the format's minimum window, where there is no log to round down to.
pub fn window_log_within(bytes: u64) -> Option<u8> {
    if bytes < (1u64 << WINDOWLOG_MIN) {
        return None;
    }
    let log = 63 - bytes.leading_zeros() as u8;
    Some(log.min(WINDOWLOG_ABSOLUTE_MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    fn matching(content_types: &[&str], extensions: &[&str]) -> Match {
        Match {
            content_types: content_types.iter().map(|s| s.to_string()).collect(),
            extensions: extensions.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn content_type_parameters_do_not_defeat_a_match() {
        let m = matching(&["text/html"], &[]);
        assert!(m.claims_content_type("text/html"));
        assert!(m.claims_content_type("text/html; charset=utf-8"));
        assert!(m.claims_content_type("Text/HTML;charset=UTF-8"));
        assert!(m.claims_content_type("  text/html  "));
        assert!(!m.claims_content_type("text/plain"));
    }

    #[test]
    fn extensions_match_case_insensitively_and_only_at_the_end() {
        let m = matching(&[], &[".webp"]);
        assert!(m.claims_filename("photo.webp"));
        assert!(m.claims_filename("PHOTO.WEBP"));
        assert!(m.claims_filename("/var/cache/photo.webp"));
        assert!(!m.claims_filename("webp"));
        assert!(!m.claims_filename("photo.webp.gz"));
    }

    #[test]
    fn a_directory_component_is_not_an_extension() {
        // The dot is in the directory, not the file. Splitting on the last dot
        // in the whole string would call this `.tar/file`.
        assert_eq!(extension_of("archive.tar/file"), None);
        assert_eq!(extension_of("/etc/rc.d/thing"), None);
    }

    #[test]
    fn a_hidden_file_has_no_extension() {
        assert_eq!(extension_of(".hidden"), None);
        assert_eq!(extension_of("trailing."), None);
        assert_eq!(extension_of("a.b"), Some(".b".to_string()));
    }

    #[test]
    fn narrowing_cannot_raise_a_host_ceiling() {
        let host = DecodeLimits {
            max_window_log: 23,
            max_decoded_size: Some(1 << 20),
        };
        let greedy = Decode {
            max_window_log: Some(31),
            max_decoded_size: Some(1 << 40),
            total_memory: Some(u64::MAX),
        };
        let out = greedy.narrow(host);
        assert_eq!(out.max_window_log, 23);
        assert_eq!(out.max_decoded_size, Some(1 << 20));
        assert_eq!(greedy.narrow_total_memory(1 << 20), 1 << 20);
    }

    #[test]
    fn narrowing_does_tighten_when_the_recipe_is_stricter() {
        let host = DecodeLimits {
            max_window_log: 31,
            max_decoded_size: None,
        };
        let strict = Decode {
            max_window_log: Some(17),
            max_decoded_size: Some(4096),
            total_memory: Some(1 << 20),
        };
        let out = strict.narrow(host);
        assert_eq!(out.max_window_log, 17);
        assert_eq!(out.max_decoded_size, Some(4096));
        assert_eq!(strict.narrow_total_memory(1 << 30), 1 << 20);
    }

    #[test]
    fn an_absent_recipe_limit_leaves_the_host_alone() {
        let host = DecodeLimits {
            max_window_log: 23,
            max_decoded_size: Some(99),
        };
        assert_eq!(Decode::default().narrow(host), host);
        assert_eq!(Decode::default().narrow_total_memory(7), 7);
    }

    #[test]
    fn window_log_rounds_down_to_stay_conservative() {
        assert_eq!(window_log_within(8 * 1024 * 1024), Some(23));
        assert_eq!(window_log_within(10 * 1024 * 1024), Some(23));
        assert_eq!(window_log_within(16 * 1024 * 1024), Some(24));
        assert_eq!(window_log_within(1024), Some(10));
        assert_eq!(window_log_within(1023), None);
        // Above what the implementation will ever accept, clamped rather than
        // overflowing the u8.
        assert_eq!(window_log_within(u64::MAX), Some(WINDOWLOG_ABSOLUTE_MAX));
    }

    #[test]
    fn the_first_matching_rule_wins_and_later_ones_do_not_apply() {
        let mut recipe = Recipe::new("test");
        recipe.encode.level = Some(6);
        recipe.rules = vec![
            Rule {
                match_: matching(&["image/webp"], &[]),
                strategy: Some(Strategy::Stored),
                level: None,
            },
            Rule {
                match_: matching(&["image/webp"], &[]),
                strategy: None,
                level: Some(19),
            },
        ];

        let hit = recipe.encode_for(Some("image/webp"), None);
        assert_eq!(hit.strategy, Strategy::Stored);
        // The second rule also claims image/webp, but the first already fired.
        assert_eq!(hit.level, Some(6));

        let miss = recipe.encode_for(Some("text/plain"), None);
        assert_eq!(miss.strategy, Strategy::Compress);
        assert_eq!(miss.level, Some(6));
    }

    #[test]
    fn a_rule_can_fire_on_a_filename_when_no_content_type_is_known() {
        let mut recipe = Recipe::new("test");
        recipe.rules = vec![Rule {
            match_: matching(&[], &[".woff2"]),
            strategy: Some(Strategy::Stored),
            level: None,
        }];
        assert_eq!(
            recipe.encode_for(None, Some("font.woff2")).strategy,
            Strategy::Stored
        );
        assert_eq!(
            recipe.encode_for(None, Some("font.ttf")).strategy,
            Strategy::Compress
        );
    }
}
