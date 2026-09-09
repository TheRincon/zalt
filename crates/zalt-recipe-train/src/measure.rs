//! Measuring a dictionary, held out.
//!
//! ## Leave-one-out, always
//!
//! Compressing a document against a dictionary that contains it measures
//! memorisation, and reports a number nothing in production will ever
//! reproduce. Every measurement here rebuilds the dictionary **without** the
//! document being measured. That makes evaluation N times more expensive and it
//! is not optional: the first version of the ZTML numbers was almost taken the
//! cheap way, and it would have reported ratios roughly double the truth.
//!
//! ## The reference does the compressing
//!
//! There is no encoder until M5, so ratio comes from the system `zstd` the same
//! way `zalt-bench` takes its baselines. That is acceptable in a dev-time tool
//! and would not be anywhere else.
//!
//! Ratios are deterministic, so unlike throughput they need no machine-state
//! caveat — the same input and level give the same bytes on any machine. This
//! is the one measurement in the project that can be quoted without one.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::dictionary;

/// The system zstd, or `$ZSTD`.
pub fn zstd_binary() -> Option<String> {
    let zstd = std::env::var("ZSTD").unwrap_or_else(|_| "zstd".into());
    Command::new(&zstd)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| zstd)
}

/// Compressed size of a file, optionally against a dictionary.
///
/// Takes a **path**, not bytes, and that is a correctness requirement rather
/// than convenience. Piping through stdin leaves the input size unknown, so the
/// frame carries no `Frame_Content_Size` and the encoder cannot size its
/// choices to a resource it has not seen the end of. Measured on this corpus
/// that costs 27-44 bytes — and it costs them almost entirely to the *baseline*,
/// because the dictionary case is dominated by long matches either way. An
/// early version of this tool piped stdin and reported every gain about one
/// percentage point too high, from a harness detail with nothing to do with
/// dictionaries.
///
/// A server compressing a resource it holds knows the length. So does this.
pub fn compressed_len(
    zstd: &str,
    level: i32,
    dictionary: Option<&Path>,
    input: &Path,
) -> std::io::Result<usize> {
    let mut cmd = Command::new(zstd);
    cmd.args(["-q", "-c", &format!("-{level}")]);
    if let Some(d) = dictionary {
        cmd.arg("-D").arg(d);
    }
    let out = cmd.arg(input).stdout(Stdio::piped()).output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "zstd failed on {}",
            input.display()
        )));
    }
    Ok(out.stdout.len())
}

/// One document's held-out result.
pub struct Row {
    pub name: String,
    pub raw: usize,
    pub plain: usize,
    pub with_dictionary: usize,
    /// Size of the leave-one-out dictionary this was measured against.
    pub dictionary_len: usize,
}

impl Row {
    /// Percentage the dictionary removed from the compressed output.
    pub fn gain(&self) -> f64 {
        if self.plain == 0 {
            return 0.0;
        }
        (self.plain as f64 - self.with_dictionary as f64) * 100.0 / self.plain as f64
    }
}

/// Leave-one-out evaluation across a corpus.
///
/// For each sample: rebuild the dictionary from every *other* sample, compress
/// the held-out document against it, and compare with compressing it alone.
pub fn leave_one_out(
    zstd: &str,
    level: i32,
    vocabulary: Option<&Path>,
    samples: &[PathBuf],
    budget: Option<usize>,
) -> std::io::Result<Vec<Row>> {
    let mut rows = Vec::with_capacity(samples.len());
    let scratch = std::env::temp_dir().join(format!("zalt-train-{}", std::process::id()));
    std::fs::create_dir_all(&scratch)?;
    let dict_path = scratch.join("loo.dict");

    for held in samples {
        let others: Vec<PathBuf> = samples.iter().filter(|s| *s != held).cloned().collect();
        let vocab = match vocabulary {
            Some(p) => Some((p, dictionary::read_vocabulary(p)?)),
            None => None,
        };
        let built = dictionary::build(vocab, &others, budget)?;
        std::fs::write(&dict_path, &built.blob)?;

        rows.push(Row {
            name: held
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            raw: std::fs::metadata(held)?.len() as usize,
            plain: compressed_len(zstd, level, None, held)?,
            with_dictionary: compressed_len(zstd, level, Some(&dict_path), held)?,
            dictionary_len: built.blob.len(),
        });
    }

    let _ = std::fs::remove_dir_all(&scratch);
    Ok(rows)
}

/// Mean held-out gain, the single number a sweep compares points by.
pub fn mean_gain(rows: &[Row]) -> f64 {
    if rows.is_empty() {
        return 0.0;
    }
    rows.iter().map(Row::gain).sum::<f64>() / rows.len() as f64
}

/// Total compressed bytes across the corpus, held out.
///
/// Reported alongside the mean because they can disagree: a mean over documents
/// weights a 3 KB file as heavily as a 12 KB one, while a total weights by
/// size. Which is the right objective depends on the workload, and §8 says this
/// one is dominated by *many small* resources — so the mean is the honest
/// headline here and the total is the sanity check.
pub fn total_with_dictionary(rows: &[Row]) -> usize {
    rows.iter().map(|r| r.with_dictionary).sum()
}
