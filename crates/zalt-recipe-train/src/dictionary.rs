//! Building a content-prefix dictionary, and recording what went into it.
//!
//! §7 stages v1 as "a concatenation of representative samples \[that\] needs no
//! algorithm at all". That is true of the *contents* and false of the
//! *arrangement*, which is the only thing this module is careful about.
//!
//! Measured on the ZTML corpus: the same bytes concatenated in a different
//! order changed the compressed output by up to 3.7%. zstd matches backwards
//! from the end of the dictionary, so material nearest the end encodes with the
//! cheapest offsets. **The order is part of the artifact**, and a generator
//! that concatenates in whatever order the filesystem enumerated produces a
//! different dictionary on a different machine, silently, with no symptom but a
//! ratio nobody compared.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use zalt_recipe::sha256::Sha256;

/// One input, and where it landed.
pub struct Part {
    /// `vocabulary:name` or `sample:name`, as the provenance file spells it.
    pub label: String,
    pub bytes: Vec<u8>,
}

/// A built dictionary and the record of how.
pub struct Built {
    pub blob: Vec<u8>,
    pub parts: Vec<Part>,
    /// Bytes dropped from the front to meet a size budget, if one applied.
    pub trimmed: usize,
}

/// Escapes honoured in a vocabulary file.
///
/// `\s` is a space, spelled explicitly so a line's trailing whitespace survives
/// an editor that strips it — the indentation-run fragments are nothing but
/// trailing whitespace.
const ESCAPES: [(&str, &str); 4] = [("\\n", "\n"), ("\\t", "\t"), ("\\s", " "), ("\\\"", "\"")];

/// Read a vocabulary file into the bytes it denotes.
///
/// One fragment per line; blank lines and `#` comments ignored; escapes above
/// honoured. Deliberately a plain list so it can be replaced wholesale by a
/// specification's own element set without touching this code.
pub fn read_vocabulary(path: &Path) -> std::io::Result<Vec<u8>> {
    let text = fs::read_to_string(path)?;
    let mut out = String::new();
    for raw in text.lines() {
        let line = raw.trim_end_matches('\n');
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let mut piece = line.to_string();
        for (from, to) in ESCAPES {
            piece = piece.replace(from, to);
        }
        out.push_str(&piece);
    }
    Ok(out.into_bytes())
}

/// Build a content prefix.
///
/// Layout, end-weighted throughout:
///
/// ```text
/// [ vocabulary fragments ][ samples, ascending size ]
///                                                   ^ end
/// ```
///
/// Samples last because they are the bulk of the value: they carry the
/// vocabulary *in context*, which matches better than a bare fragment. Ascending
/// size puts the largest and richest sample nearest the end. Sorting by name
/// instead would make the order an accident of what things are called.
///
/// `budget`, when set, drops bytes from the **front**. That direction is the
/// measured one: the front is the vocabulary and the smallest samples, and the
/// tail is where the cheap offsets are. Trimming the tail would discard exactly
/// the material a size-constrained dictionary most needs to keep.
pub fn build(
    vocabulary: Option<(&Path, Vec<u8>)>,
    samples: &[PathBuf],
    budget: Option<usize>,
) -> std::io::Result<Built> {
    let mut parts: Vec<Part> = Vec::new();

    if let Some((path, bytes)) = vocabulary {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        parts.push(Part {
            label: format!("vocabulary:{name}"),
            bytes,
        });
    }

    let mut sized: Vec<(u64, PathBuf)> = Vec::with_capacity(samples.len());
    for path in samples {
        sized.push((fs::metadata(path)?.len(), path.clone()));
    }
    // Ascending size, then name: deterministic, and independent of the order
    // the caller happened to list them in.
    sized.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.file_name().cmp(&b.1.file_name()))
    });

    for (_, path) in &sized {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        parts.push(Part {
            label: format!("sample:{name}"),
            bytes: fs::read(path)?,
        });
    }

    let mut blob: Vec<u8> = parts.iter().flat_map(|p| p.bytes.iter().copied()).collect();
    let mut trimmed = 0usize;
    if let Some(limit) = budget
        && blob.len() > limit
    {
        trimmed = blob.len() - limit;
        blob.drain(..trimmed);
    }

    Ok(Built {
        blob,
        parts,
        trimmed,
    })
}

/// The provenance record written beside a dictionary.
///
/// A dictionary whose bytes no longer match its inputs is one nobody can
/// regenerate, which makes every ratio measured against it unreproducible. This
/// file is what `just ztml-dict-check` compares against.
pub fn provenance(built: &Built) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# ZTML content-prefix dictionary — provenance");
    let _ = writeln!(s, "#");
    let _ = writeln!(
        s,
        "# Generated by zalt-recipe-train. Do not edit either file by hand:"
    );
    let _ = writeln!(
        s,
        "# run `just ztml-dict` and commit the result. `just ztml-dict-check`"
    );
    let _ = writeln!(
        s,
        "# re-derives the blob and fails if it differs, so the checked-in bytes"
    );
    let _ = writeln!(s, "# are always exactly what these inputs produce.");
    let _ = writeln!(s, "#");
    let _ = writeln!(
        s,
        "# Order is significant and is the generator's, not the filesystem's."
    );
    let _ = writeln!(s);

    let digest = Sha256::digest(&built.blob);
    let hex = String::from_utf8_lossy(&digest.to_hex()).to_string();
    let _ = writeln!(s, "sha256  {hex}");
    let _ = writeln!(s, "bytes   {}", built.blob.len());
    if built.trimmed > 0 {
        let _ = writeln!(
            s,
            "trimmed {} bytes from the front to meet a size budget",
            built.trimmed
        );
    }
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "# inputs, in concatenation order (offset, bytes, sha256, name)"
    );

    let mut offset = 0usize;
    for part in &built.parts {
        let d = Sha256::digest(&part.bytes);
        let short = String::from_utf8_lossy(&d.to_hex()[..16]).to_string();
        let _ = writeln!(
            s,
            "{:<8} {:<8} {}  {}",
            offset,
            part.bytes.len(),
            short,
            part.label
        );
        offset += part.bytes.len();
    }
    s
}
