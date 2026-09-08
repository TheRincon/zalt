//! Error taxonomy for manifests.
//!
//! Unlike [`zalt_codec::error::Error`], these carry a **line number**. The two
//! types answer different questions and can afford different shapes: a codec
//! error is a per-frame result that crosses a WASM boundary millions of times
//! and must stay a scalar, while a manifest error is a load-time configuration
//! failure a human is going to read once and then go fix. Telling them the line
//! is most of the value, so the type pays a `u32` for it and stays `Copy`
//! regardless.
//!
//! What it deliberately does *not* carry is the offending text. An owned
//! `String` per error would make the type allocate to describe a typo, and the
//! line number already points at the word.

use core::fmt;

/// Result alias for manifest operations.
pub type Result<T> = core::result::Result<T, Error>;

/// What went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// A line was neither a comment, a table header, nor `key = value`.
    Malformed,
    /// A table header was not closed, or nested beyond one level.
    BadTableHeader,
    /// A table name this crate does not define.
    ///
    /// Refused rather than ignored. A manifest whose `[encoder]` table silently
    /// does nothing because the name is `[encode]` is a tuning change that
    /// appears to land and does not, which is worse than a failed load.
    UnknownTable,
    /// A key this crate does not define, in an otherwise valid table.
    UnknownKey,
    /// The same key, or the same non-array table, appeared twice.
    Duplicate,
    /// A value of the wrong type for its key.
    TypeMismatch,
    /// A string literal was unterminated, or carried an unsupported escape.
    BadString,
    /// A number was unparseable, or overflowed the field it was destined for.
    BadNumber,
    /// A size string carried a suffix that is not a byte unit.
    BadSize,
    /// TOML this parser deliberately does not implement.
    ///
    /// Floats, datetimes, dotted keys, inline tables, literal and multi-line
    /// strings, nested arrays. Refused explicitly rather than mis-read: a
    /// partial parser that guesses at syntax it does not support is worse than
    /// one that stops, because the guess becomes a wrong tuning parameter that
    /// nobody looks at again.
    Unsupported,
    /// A value was well-formed but outside the range its field permits.
    OutOfRange,
    /// The manifest omitted something with no sensible default.
    Missing,
    /// A `[[rule]]` matched nothing, so it could never fire.
    ///
    /// Almost certainly a mistake, and a silent one: the rule sits in the file
    /// looking like it works.
    EmptyRule,
    /// Two recipes in a set claimed the same content type or extension.
    ///
    /// Refused because routing must be deterministic, and "whichever loaded
    /// first" is not a rule anyone can reason about from the manifests alone.
    AmbiguousRoute,
    /// The dictionary's own `Dictionary_ID` disagreed with the manifest.
    DictionaryIdMismatch,
    /// The dictionary bytes were not a valid dictionary.
    BadDictionary,
    /// The manifest or dictionary could not be read from the filesystem.
    Io,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Malformed => "not a comment, a table header, or `key = value`",
            Self::BadTableHeader => "malformed table header",
            Self::UnknownTable => "unknown table",
            Self::UnknownKey => "unknown key",
            Self::Duplicate => "duplicate key or table",
            Self::TypeMismatch => "wrong value type for this key",
            Self::BadString => "malformed string",
            Self::BadNumber => "malformed number",
            Self::BadSize => "malformed size: expected bytes or a K/M/G suffix",
            Self::Unsupported => "TOML syntax this parser does not implement",
            Self::OutOfRange => "value out of range",
            Self::Missing => "required key missing",
            Self::EmptyRule => "rule matches nothing and can never fire",
            Self::AmbiguousRoute => "two recipes claim the same route",
            Self::DictionaryIdMismatch => "dictionary id does not match the manifest",
            Self::BadDictionary => "not a valid dictionary",
            Self::Io => "could not be read",
        })
    }
}

/// A manifest failure, and where it happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error {
    /// What went wrong.
    pub kind: ErrorKind,
    /// 1-based line in the manifest, or 0 where no line applies — a missing
    /// required key belongs to the file, not to any line in it.
    pub line: u32,
}

impl Error {
    /// An error at a known line.
    pub const fn at(line: u32, kind: ErrorKind) -> Self {
        Self { kind, line }
    }

    /// An error belonging to the file as a whole.
    pub const fn whole_file(kind: ErrorKind) -> Self {
        Self { kind, line: 0 }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(f, "{}", self.kind)
        } else {
            write!(f, "line {}: {}", self.line, self.kind)
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Error {}
