//! A manifest parser for the TOML subset §7's sketch uses — and nothing else.
//!
//! ## Why this is hand-written
//!
//! The workspace has no third-party runtime dependencies, and `serde` + `toml`
//! to read a six-table configuration file would be the largest dependency event
//! in the project by an order of magnitude. `zalt-cli/src/args.rs` already took
//! this position for the command line; a manifest is the same shape of problem.
//!
//! ## Why it refuses so much
//!
//! Everything below is a *deliberate* subset:
//!
//! - basic strings `"..."` with `\\ \" \n \r \t` escapes
//! - integers, decimal or `0x` hex, with `_` separators, optionally signed
//! - booleans
//! - flat arrays of strings, which may span lines
//! - `[table]` and `[[array of tables]]`, one level deep
//!
//! Floats, datetimes, dotted keys, inline tables, literal strings, multi-line
//! strings and nested arrays are **refused with [`ErrorKind::Unsupported`]**,
//! not skipped and not guessed at.
//!
//! That distinction is the whole design. A partial parser that quietly
//! mis-reads syntax it does not implement produces a manifest that loads
//! cleanly and tunes nothing, or tunes the wrong thing — and a tuning parameter
//! that silently fails to apply is invisible, because the only symptom is a
//! ratio nobody measured against the alternative. Refusing at load time makes
//! that a five-second fix instead of a benchmark mystery.
//!
//! Unknown *tables* and unknown *keys* are refused for the same reason. A
//! manifest whose `[encoder]` table does nothing because the name is `[encode]`
//! has failed, and should say so.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::{Error, ErrorKind, Result};
use crate::recipe::{
    DictionaryRef, DictionarySource, LEVEL_MAX, LEVEL_MIN, Recipe, Rule, Strategy,
    normalise_content_type, normalise_extension, window_log_within,
};
use zalt_codec::params::{WINDOWLOG_ABSOLUTE_MAX, WINDOWLOG_MIN};

/// Largest number of `[[rule]]` tables one manifest may carry.
///
/// Not a resource bound — a manifest is not hostile input in the sense a frame
/// is. It exists so the per-rule namespace used by the duplicate-key ledger
/// cannot overflow, and 256 rules in one recipe is already a manifest that
/// wants splitting.
pub const MAX_RULES: usize = 256;

/// Parse a manifest.
pub fn parse(text: &str) -> Result<Recipe> {
    Parser::new(text).run()
}

/// A parsed scalar. Arrays are handled as a case rather than as a general value
/// tree, because only arrays of strings appear anywhere in this format and a
/// recursive value type would be more machinery than six tables can justify.
enum Value {
    Str(String),
    Int(i64),
    /// Recognised but carries nothing, because no key in this format accepts a
    /// boolean. It exists so `strategy = true` reports a type mismatch instead
    /// of a malformed number, which is a better description of that mistake.
    Bool,
    List(Vec<String>),
}

/// Which table the parser is currently inside.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Table {
    /// Before any header. Keys here are an error; this format has no top level.
    None,
    Recipe,
    Match,
    Dictionary,
    Encode,
    Decode,
    /// The most recent `[[rule]]`, by index into `rules`.
    Rule(usize),
}

impl Table {
    /// A namespace for the duplicate-key ledger.
    ///
    /// Each `[[rule]]` gets its own, so two rules may both set `strategy`
    /// without colliding with each other.
    fn namespace(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Recipe => 1,
            Self::Match => 2,
            Self::Dictionary => 3,
            Self::Encode => 4,
            Self::Decode => 5,
            Self::Rule(index) => 6 + index as u32,
        }
    }
}

struct Parser<'a> {
    text: &'a str,
    recipe: Recipe,
    table: Table,
    line: u32,
    /// Which single-value tables have been opened, so a second `[encode]` is a
    /// duplicate rather than a silent continuation of the first.
    seen: [bool; 5],
    /// Keys already set, as `(namespace, key)`. A vector rather than a map:
    /// there are thirteen keys in the whole format, and a hash map would be
    /// more code than the linear scan it replaces.
    assigned: Vec<(u32, &'static str)>,
    /// `max_window` and `max_window_log` are two spellings of one field, so the
    /// key-name ledger cannot catch a manifest that sets both. This does.
    window_spelling_seen: bool,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            recipe: Recipe::new(""),
            table: Table::None,
            line: 0,
            seen: [false; 5],
            assigned: Vec::new(),
            window_spelling_seen: false,
        }
    }

    fn err(&self, kind: ErrorKind) -> Error {
        Error::at(self.line, kind)
    }

    fn run(mut self) -> Result<Recipe> {
        // An array may span lines, so the loop drives an iterator it can pull
        // continuation lines from rather than a plain `for`.
        let mut lines = self.text.lines().enumerate();
        while let Some((index, raw)) = lines.next() {
            self.line = index as u32 + 1;
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            match line.strip_prefix('[') {
                Some(rest) => self.table_header(rest)?,
                None => self.assignment(line, &mut lines)?,
            }
        }
        self.finish()
    }

    /// `rest` is everything after the opening `[`.
    fn table_header(&mut self, rest: &str) -> Result<()> {
        let (name, is_array) = match rest.strip_prefix('[') {
            Some(inner) => (
                inner
                    .strip_suffix("]]")
                    .ok_or_else(|| self.err(ErrorKind::BadTableHeader))?,
                true,
            ),
            None => (
                rest.strip_suffix(']')
                    .ok_or_else(|| self.err(ErrorKind::BadTableHeader))?,
                false,
            ),
        };
        let name = name.trim();
        if name.is_empty() || name.contains('[') || name.contains(']') {
            return Err(self.err(ErrorKind::BadTableHeader));
        }
        if name.contains('.') {
            // `[a.b]` is valid TOML, and this format has no nesting, so it can
            // only be a misunderstanding of the schema.
            return Err(self.err(ErrorKind::Unsupported));
        }

        if is_array {
            if name != "rule" {
                return Err(self.err(ErrorKind::UnknownTable));
            }
            if self.recipe.rules.len() == MAX_RULES {
                return Err(self.err(ErrorKind::OutOfRange));
            }
            self.recipe.rules.push(Rule::default());
            self.table = Table::Rule(self.recipe.rules.len() - 1);
            return Ok(());
        }

        let (table, slot) = match name {
            "recipe" => (Table::Recipe, 0),
            "match" => (Table::Match, 1),
            "dictionary" => (Table::Dictionary, 2),
            "encode" => (Table::Encode, 3),
            "decode" => (Table::Decode, 4),
            // `[rule]` written without the doubled brackets is the mistake this
            // format invites most. It would otherwise read as an unknown table,
            // which is true and unhelpful.
            "rule" => return Err(self.err(ErrorKind::BadTableHeader)),
            _ => return Err(self.err(ErrorKind::UnknownTable)),
        };
        if self.seen[slot] {
            return Err(self.err(ErrorKind::Duplicate));
        }
        self.seen[slot] = true;
        self.table = table;
        Ok(())
    }

    fn assignment<I>(&mut self, line: &str, lines: &mut I) -> Result<()>
    where
        I: Iterator<Item = (usize, &'a str)>,
    {
        let (key, rest) = line
            .split_once('=')
            .ok_or_else(|| self.err(ErrorKind::Malformed))?;
        let key = key.trim();
        if key.is_empty() {
            return Err(self.err(ErrorKind::Malformed));
        }
        if key.contains('.') || key.starts_with('"') || key.starts_with('\'') {
            return Err(self.err(ErrorKind::Unsupported));
        }
        if !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(self.err(ErrorKind::Malformed));
        }
        let value = self.value(rest.trim(), lines)?;
        self.assign(key, value)
    }

    /// Parse a value, pulling further lines if it is an array that continues.
    fn value<I>(&mut self, text: &str, lines: &mut I) -> Result<Value>
    where
        I: Iterator<Item = (usize, &'a str)>,
    {
        let Some(&first) = text.as_bytes().first() else {
            return Err(self.err(ErrorKind::Malformed));
        };
        match first {
            b'[' => self.array(&text[1..], lines),
            b'"' if text.starts_with("\"\"\"") => Err(self.err(ErrorKind::Unsupported)),
            b'"' => {
                let (s, rest) = self.basic_string(text)?;
                if !rest.trim().is_empty() {
                    return Err(self.err(ErrorKind::Malformed));
                }
                Ok(Value::Str(s))
            }
            // Literal strings and inline tables. Both real TOML, neither needed
            // by any key here.
            b'\'' | b'{' => Err(self.err(ErrorKind::Unsupported)),
            _ => match text {
                "true" | "false" => Ok(Value::Bool),
                word => Ok(Value::Int(self.integer(word)?)),
            },
        }
    }

    /// `rest` is everything after the opening `[`.
    fn array<I>(&mut self, rest: &str, lines: &mut I) -> Result<Value>
    where
        I: Iterator<Item = (usize, &'a str)>,
    {
        let mut items = Vec::new();
        let mut rest = rest;
        loop {
            let text = strip_comment(rest).trim_start();
            if let Some(after) = text.strip_prefix(']') {
                if !after.trim().is_empty() {
                    return Err(self.err(ErrorKind::Malformed));
                }
                return Ok(Value::List(items));
            }
            if text.is_empty() {
                // Continue onto the next line. An array left open at end of file
                // falls out here as malformed, rather than as a list silently
                // truncated at whatever had been collected.
                let (index, next) = lines.next().ok_or_else(|| self.err(ErrorKind::Malformed))?;
                self.line = index as u32 + 1;
                rest = next;
                continue;
            }
            if text.starts_with('[') || text.starts_with('{') {
                return Err(self.err(ErrorKind::Unsupported));
            }
            if !text.starts_with('"') {
                // Every array in this format is an array of strings, so an
                // array of integers is a type error rather than a syntax one.
                return Err(self.err(ErrorKind::TypeMismatch));
            }
            let (item, after) = self.basic_string(text)?;
            items.push(item);

            let after = strip_comment(after).trim_start();
            rest = match after.strip_prefix(',') {
                Some(more) => more,
                // No comma: either the closing bracket is next, or it is on a
                // later line and this is the last item. Anything else is two
                // values with nothing between them.
                None if after.is_empty() || after.starts_with(']') => after,
                None => return Err(self.err(ErrorKind::Malformed)),
            };
        }
    }

    /// Parse one basic string, returning it and whatever followed its closing
    /// quote. `text` starts at the opening quote.
    fn basic_string<'t>(&self, text: &'t str) -> Result<(String, &'t str)> {
        let body = &text[1..];
        let mut out = String::new();
        let mut chars = body.char_indices();
        while let Some((offset, c)) = chars.next() {
            match c {
                '"' => return Ok((out, &body[offset + 1..])),
                '\\' => {
                    let (_, escape) = chars.next().ok_or_else(|| self.err(ErrorKind::BadString))?;
                    out.push(match escape {
                        '\\' => '\\',
                        '"' => '"',
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        // Unicode escapes are real TOML and are not needed by
                        // any key in this format. Refused rather than passed
                        // through as the literal characters that follow the
                        // backslash, which is what dropping it would produce.
                        _ => return Err(self.err(ErrorKind::BadString)),
                    });
                }
                other => out.push(other),
            }
        }
        Err(self.err(ErrorKind::BadString))
    }

    fn integer(&self, word: &str) -> Result<i64> {
        if word.contains('.') || word.contains(':') {
            // A float, or a datetime. Both better named than "bad number".
            return Err(self.err(ErrorKind::Unsupported));
        }
        let (negative, rest) = match word.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, word.strip_prefix('+').unwrap_or(word)),
        };
        let (radix, digits) = match rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
            Some(hex) => (16, hex),
            None => (10, rest),
        };
        let cleaned: String = digits.chars().filter(|c| *c != '_').collect();
        if cleaned.is_empty() || !cleaned.chars().all(|c| c.is_digit(radix)) {
            return Err(self.err(ErrorKind::BadNumber));
        }
        // Parsed unsigned first, so that `0xFFFFFFFF` — how a Dictionary_ID
        // gets written — is not an overflow on the way to a u32 field.
        let magnitude =
            u64::from_str_radix(&cleaned, radix).map_err(|_| self.err(ErrorKind::BadNumber))?;
        if negative {
            if magnitude > i64::MAX as u64 + 1 {
                return Err(self.err(ErrorKind::BadNumber));
            }
            Ok((magnitude as i64).wrapping_neg())
        } else {
            i64::try_from(magnitude).map_err(|_| self.err(ErrorKind::BadNumber))
        }
    }

    fn assign(&mut self, key: &str, value: Value) -> Result<()> {
        let table = self.table;
        if table == Table::None {
            return Err(self.err(ErrorKind::UnknownTable));
        }

        // Each arm yields the `'static` spelling of the key it handled, which
        // is what the duplicate ledger stores: the `key` borrowed from the
        // input line does not outlive this call, and returning the name from
        // the arm that matched it avoids a second lookup table that could drift
        // out of step with this one.
        let name: &'static str = match (table, key) {
            (Table::Recipe, "name") => {
                let name = self.expect_string(value)?;
                if name.trim().is_empty() {
                    return Err(self.err(ErrorKind::Missing));
                }
                self.recipe.name = name;
                "name"
            }
            (Table::Match, "content_type") => {
                self.recipe.match_.content_types = self.expect_content_types(value)?;
                "content_type"
            }
            (Table::Match, "extension") => {
                self.recipe.match_.extensions = self.expect_extensions(value)?;
                "extension"
            }
            (Table::Dictionary, "path") => {
                let path = self.expect_string(value)?;
                if path.trim().is_empty() {
                    return Err(self.err(ErrorKind::Missing));
                }
                self.dictionary().source = DictionarySource::Path(path);
                "path"
            }
            (Table::Dictionary, "id") => {
                let id = self.expect_u32(value)?;
                self.dictionary().id = Some(id);
                "id"
            }
            (Table::Encode, "level") => {
                self.recipe.encode.level = Some(self.expect_level(value)?);
                "level"
            }
            (Table::Encode, "window_log") => {
                self.recipe.encode.window_log = Some(self.expect_window_log(value)?);
                "window_log"
            }
            (Table::Encode, "strategy") => {
                self.recipe.encode.strategy = self.expect_strategy(value)?;
                "strategy"
            }
            (Table::Decode, "max_window") => {
                let bytes = self.expect_size(value)?;
                let log =
                    window_log_within(bytes).ok_or_else(|| self.err(ErrorKind::OutOfRange))?;
                self.claim_window_spelling()?;
                self.recipe.decode.max_window_log = Some(log);
                "max_window"
            }
            (Table::Decode, "max_window_log") => {
                let log = self.expect_window_log(value)?;
                self.claim_window_spelling()?;
                self.recipe.decode.max_window_log = Some(log);
                "max_window_log"
            }
            (Table::Decode, "max_decoded") => {
                self.recipe.decode.max_decoded_size = Some(self.expect_size(value)?);
                "max_decoded"
            }
            (Table::Decode, "total_memory") => {
                self.recipe.decode.total_memory = Some(self.expect_size(value)?);
                "total_memory"
            }
            (Table::Rule(index), "match_content_type") => {
                self.recipe.rules[index].match_.content_types = self.expect_content_types(value)?;
                "match_content_type"
            }
            (Table::Rule(index), "match_extension") => {
                self.recipe.rules[index].match_.extensions = self.expect_extensions(value)?;
                "match_extension"
            }
            (Table::Rule(index), "strategy") => {
                self.recipe.rules[index].strategy = Some(self.expect_strategy(value)?);
                "strategy"
            }
            (Table::Rule(index), "level") => {
                self.recipe.rules[index].level = Some(self.expect_level(value)?);
                "level"
            }
            _ => return Err(self.err(ErrorKind::UnknownKey)),
        };

        // Checked after assignment rather than before, so that a repeated key
        // whose value is *also* wrong reports the wrong value. That is the
        // thing the author has to fix either way, and reporting the duplicate
        // first would hide it until the next run.
        let namespace = table.namespace();
        if self.assigned.contains(&(namespace, name)) {
            return Err(self.err(ErrorKind::Duplicate));
        }
        self.assigned.push((namespace, name));
        Ok(())
    }

    fn dictionary(&mut self) -> &mut DictionaryRef {
        self.recipe.dictionary.get_or_insert(DictionaryRef {
            id: None,
            source: DictionarySource::ById,
        })
    }

    fn claim_window_spelling(&mut self) -> Result<()> {
        if self.window_spelling_seen {
            return Err(self.err(ErrorKind::Duplicate));
        }
        self.window_spelling_seen = true;
        Ok(())
    }

    fn expect_string(&self, value: Value) -> Result<String> {
        match value {
            Value::Str(s) => Ok(s),
            _ => Err(self.err(ErrorKind::TypeMismatch)),
        }
    }

    fn expect_list(&self, value: Value) -> Result<Vec<String>> {
        match value {
            Value::List(items) => Ok(items),
            // A bare string where a list belongs is the likeliest typo here.
            // Accepting it would make the schema ambiguous for no gain.
            _ => Err(self.err(ErrorKind::TypeMismatch)),
        }
    }

    fn expect_content_types(&self, value: Value) -> Result<Vec<String>> {
        let items = self.expect_list(value)?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let normalised = normalise_content_type(&item);
            if normalised.is_empty() {
                return Err(self.err(ErrorKind::Missing));
            }
            out.push(normalised);
        }
        Ok(out)
    }

    fn expect_extensions(&self, value: Value) -> Result<Vec<String>> {
        let items = self.expect_list(value)?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let normalised = normalise_extension(&item);
            if normalised == "." {
                return Err(self.err(ErrorKind::Missing));
            }
            out.push(normalised);
        }
        Ok(out)
    }

    fn expect_int(&self, value: Value) -> Result<i64> {
        match value {
            Value::Int(n) => Ok(n),
            _ => Err(self.err(ErrorKind::TypeMismatch)),
        }
    }

    fn expect_u32(&self, value: Value) -> Result<u32> {
        let n = self.expect_int(value)?;
        u32::try_from(n).map_err(|_| self.err(ErrorKind::OutOfRange))
    }

    fn expect_level(&self, value: Value) -> Result<i32> {
        let n = self.expect_int(value)?;
        if !(i64::from(LEVEL_MIN)..=i64::from(LEVEL_MAX)).contains(&n) {
            return Err(self.err(ErrorKind::OutOfRange));
        }
        Ok(n as i32)
    }

    fn expect_window_log(&self, value: Value) -> Result<u8> {
        let n = self.expect_int(value)?;
        if !(i64::from(WINDOWLOG_MIN)..=i64::from(WINDOWLOG_ABSOLUTE_MAX)).contains(&n) {
            return Err(self.err(ErrorKind::OutOfRange));
        }
        Ok(n as u8)
    }

    fn expect_strategy(&self, value: Value) -> Result<Strategy> {
        match self.expect_string(value)?.as_str() {
            "compress" => Ok(Strategy::Compress),
            "stored" => Ok(Strategy::Stored),
            _ => Err(self.err(ErrorKind::OutOfRange)),
        }
    }

    /// A byte count, written as a size string or as a bare integer.
    ///
    /// Both, because `total_memory = 268435456` and `total_memory = "256MiB"`
    /// are the same intent and an author should not have to remember which one
    /// this file wants.
    fn expect_size(&self, value: Value) -> Result<u64> {
        match value {
            Value::Int(n) => u64::try_from(n).map_err(|_| self.err(ErrorKind::OutOfRange)),
            Value::Str(s) => parse_size(&s).map_err(|kind| self.err(kind)),
            _ => Err(self.err(ErrorKind::TypeMismatch)),
        }
    }

    fn finish(self) -> Result<Recipe> {
        if self.recipe.name.is_empty() {
            // Nothing else here is required — a recipe that only sets a decode
            // ceiling is a legitimate thing to write — but an unnamed one
            // cannot be named in a diagnostic or selected by name.
            return Err(Error::whole_file(ErrorKind::Missing));
        }
        if self.recipe.rules.iter().any(|r| r.match_.is_empty()) {
            return Err(Error::whole_file(ErrorKind::EmptyRule));
        }
        Ok(self.recipe)
    }
}

/// Byte counts, with the suffixes people actually type.
///
/// `KB` and `KiB` both mean 1024. Being pedantic about SI would be technically
/// correct and would surprise every author, and the difference does not matter
/// for a memory ceiling. This deliberately matches `zalt-cli`'s `--budget`
/// parsing: two places a size is written by a human, one set of rules between
/// them.
pub fn parse_size(s: &str) -> core::result::Result<u64, ErrorKind> {
    let t = s.trim();
    let digits_end = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    if digits_end == 0 {
        return Err(ErrorKind::BadSize);
    }
    let (num, suffix) = t.split_at(digits_end);
    let n: u64 = num.parse().map_err(|_| ErrorKind::OutOfRange)?;
    let scale: u64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        _ => return Err(ErrorKind::BadSize),
    };
    n.checked_mul(scale).ok_or(ErrorKind::OutOfRange)
}

/// Strip a `#` comment, respecting quotes.
///
/// A naive `split('#')` would truncate `content_type = ["text/x-c#"]`, which
/// nobody will write — but the same naivety also truncates a dictionary path
/// containing a `#`, which is an ordinary filename.
fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in line.as_bytes().iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match b {
            b'\\' if in_string => escaped = true,
            b'"' => in_string = !in_string,
            b'#' if !in_string => return &line[..i],
            _ => {}
        }
    }
    line
}
