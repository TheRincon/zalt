//! `zalt-recipe-train` — L3's dev-time tool.
//!
//! Builds content-prefix dictionaries and measures what they are worth, so the
//! numbers in a manifest are measured rather than guessed. §7 puts training on
//! the encode side and off the browser's critical path entirely; this binary is
//! that side, and nothing ships it.
//!
//! ```text
//! zalt-recipe-train build --vocabulary V --out DIR --name N [--max-size S] samples...
//! zalt-recipe-train eval  --vocabulary V [--level N] [--max-size S] samples...
//! zalt-recipe-train sweep --vocabulary V samples...
//! ```
//!
//! ## v1 is a content prefix, and the measurement says keep it that way
//!
//! §7 stages "content-prefix v1, then trained dictionaries". Measured on the
//! ZTML corpus, `zstd --train` **loses** to plain concatenation on five of six
//! documents: COVER needs many samples to select from, and six documents
//! totalling 37 KB is not many. So this tool implements the concatenation
//! properly — ordering, size budget, held-out evaluation — rather than
//! implementing COVER against a corpus too small for it to beat `cat`.
//!
//! When a corpus arrives that is large enough for training to win, `sweep` is
//! where that will show up: it is the harness that would demonstrate it.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod dictionary;
mod manifest;
mod measure;

const USAGE: &str = "\
zalt-recipe-train — build and measure content-prefix dictionaries

USAGE
    zalt-recipe-train <command> [options] <sample>...

COMMANDS
    build     write a dictionary and its provenance
    eval      leave-one-out ratio for a dictionary built from these samples
    sweep     search dictionary size and compression level, held out

OPTIONS
    --vocabulary PATH   fragment list placed at the front of the dictionary
    --out DIR           where build writes (default: the working directory)
    --name NAME         basename for the artifacts (default: recipe)
    --manifest PATH     also write a starter manifest; never overwrites
    --max-size SIZE     trim the dictionary to this many bytes, from the front
    --level N           compression level for eval (default 6)

SIZES
    Plain bytes, or a suffix: 4096, 32K, 1MiB. KB and KiB both mean 1024.

NOTE
    eval and sweep measure with the system zstd ($ZSTD overrides), because
    there is no encoder yet. Every measurement is leave-one-out: the dictionary
    is rebuilt without the document being measured, since compressing a file
    against a dictionary containing it measures memorisation.
";

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match run(&argv) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage(m)) => {
            eprintln!("zalt-recipe-train: {m}\n\n{USAGE}");
            ExitCode::from(2)
        }
        Err(Failure::Run(m)) => {
            eprintln!("zalt-recipe-train: {m}");
            ExitCode::FAILURE
        }
    }
}

enum Failure {
    Usage(String),
    Run(String),
}

fn io(e: std::io::Error) -> Failure {
    Failure::Run(e.to_string())
}

struct Opts {
    command: String,
    vocabulary: Option<PathBuf>,
    out: PathBuf,
    name: String,
    manifest: Option<PathBuf>,
    max_size: Option<usize>,
    level: i32,
    samples: Vec<PathBuf>,
}

fn parse(argv: &[String]) -> Result<Opts, Failure> {
    let mut o = Opts {
        command: String::new(),
        vocabulary: None,
        out: PathBuf::from("."),
        name: "recipe".into(),
        manifest: None,
        max_size: None,
        level: 6,
        samples: Vec::new(),
    };
    let mut it = argv.iter().peekable();
    let take = |flag: &str, it: &mut std::iter::Peekable<std::slice::Iter<String>>| {
        it.next()
            .cloned()
            .ok_or_else(|| Failure::Usage(format!("{flag} requires a value")))
    };

    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" | "help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "--vocabulary" => o.vocabulary = Some(PathBuf::from(take("--vocabulary", &mut it)?)),
            "--out" => o.out = PathBuf::from(take("--out", &mut it)?),
            "--name" => o.name = take("--name", &mut it)?,
            "--manifest" => o.manifest = Some(PathBuf::from(take("--manifest", &mut it)?)),
            "--max-size" => o.max_size = Some(parse_size(&take("--max-size", &mut it)?)?),
            "--level" => {
                let raw = take("--level", &mut it)?;
                o.level = raw
                    .parse()
                    .map_err(|_| Failure::Usage(format!("--level: not a number: {raw}")))?;
            }
            other if other.starts_with("--") => {
                return Err(Failure::Usage(format!("unknown option: {other}")));
            }
            word if o.command.is_empty() => o.command = word.to_string(),
            path => o.samples.push(PathBuf::from(path)),
        }
    }
    Ok(o)
}

fn parse_size(s: &str) -> Result<usize, Failure> {
    let t = s.trim();
    let end = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    if end == 0 {
        return Err(Failure::Usage(format!("not a size: {s}")));
    }
    let (num, suffix) = t.split_at(end);
    let n: usize = num
        .parse()
        .map_err(|_| Failure::Usage(format!("size out of range: {s}")))?;
    let scale = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        other => return Err(Failure::Usage(format!("unknown size suffix: {other:?}"))),
    };
    n.checked_mul(scale)
        .ok_or_else(|| Failure::Usage(format!("size out of range: {s}")))
}

fn run(argv: &[String]) -> Result<(), Failure> {
    if argv.is_empty() {
        println!("{USAGE}");
        return Ok(());
    }
    let o = parse(argv)?;
    if o.samples.is_empty() {
        return Err(Failure::Usage("no samples given".into()));
    }
    for s in &o.samples {
        if !s.is_file() {
            return Err(Failure::Run(format!("not a file: {}", s.display())));
        }
    }

    match o.command.as_str() {
        "build" => build(&o),
        "eval" => eval(&o),
        "sweep" => sweep(&o),
        "" => Err(Failure::Usage("no command given".into())),
        other => Err(Failure::Usage(format!("unknown command: {other}"))),
    }
}

fn vocabulary_of(o: &Opts) -> Result<Option<(&Path, Vec<u8>)>, Failure> {
    match &o.vocabulary {
        Some(p) => {
            let bytes = dictionary::read_vocabulary(p).map_err(io)?;
            Ok(Some((p.as_path(), bytes)))
        }
        None => Ok(None),
    }
}

fn build(o: &Opts) -> Result<(), Failure> {
    let built = dictionary::build(vocabulary_of(o)?, &o.samples, o.max_size).map_err(io)?;

    std::fs::create_dir_all(&o.out).map_err(io)?;
    let dict_path = o.out.join(format!("{}.dict", o.name));
    let prov_path = o.out.join(format!("{}.provenance.txt", o.name));
    std::fs::write(&dict_path, &built.blob).map_err(io)?;
    std::fs::write(&prov_path, dictionary::provenance(&built)).map_err(io)?;

    if let Some(path) = &o.manifest {
        // Never over an existing manifest. A hand-written one carries the prose
        // explaining why each number is what it is, and regenerating it would
        // silently throw that away — the one irreversible thing this tool
        // could do.
        if path.exists() {
            return Err(Failure::Run(format!(
                "{} exists; refusing to overwrite a manifest",
                path.display()
            )));
        }
        let mut recipe = zalt_recipe::Recipe::new(&o.name);
        recipe.dictionary = Some(zalt_recipe::DictionaryRef {
            id: None,
            source: zalt_recipe::DictionarySource::Path(
                dict_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            ),
        });
        recipe.encode.level = Some(o.level);
        std::fs::write(path, manifest::emit(&recipe)).map_err(io)?;
        println!("{}", path.display());
    }

    let digest = zalt_recipe::sha256::Sha256::digest(&built.blob);
    println!(
        "{}: {} bytes from {} inputs",
        dict_path.display(),
        built.blob.len(),
        built.parts.len()
    );
    if built.trimmed > 0 {
        println!("trimmed {} bytes from the front", built.trimmed);
    }
    println!("sha256 {}", String::from_utf8_lossy(&digest.to_hex()));
    Ok(())
}

fn eval(o: &Opts) -> Result<(), Failure> {
    let zstd = measure::zstd_binary()
        .ok_or_else(|| Failure::Run("need the zstd binary on PATH (or $ZSTD)".into()))?;
    if o.samples.len() < 2 {
        return Err(Failure::Run(
            "leave-one-out needs at least two samples: with one, the dictionary \
             would be empty"
                .into(),
        ));
    }

    let rows = measure::leave_one_out(
        &zstd,
        o.level,
        o.vocabulary.as_deref(),
        &o.samples,
        o.max_size,
    )
    .map_err(io)?;

    println!(
        "held out, level {}, dictionary {} bytes",
        o.level,
        rows.first().map_or(0, |r| r.dictionary_len)
    );
    println!(
        "{:<22} {:>8} {:>9} {:>9} {:>8}",
        "document", "bytes", "plain", "dict", "gain"
    );
    for r in &rows {
        println!(
            "{:<22} {:>8} {:>9} {:>9} {:>7.1}%",
            r.name,
            r.raw,
            r.plain,
            r.with_dictionary,
            r.gain()
        );
    }
    println!(
        "\nmean gain {:.1}%, corpus total {} bytes",
        measure::mean_gain(&rows),
        measure::total_with_dictionary(&rows)
    );
    Ok(())
}

/// Search level and dictionary size, held out.
///
/// The parameter set §7 promises. Levels span the range a recipe would
/// plausibly choose; sizes span a browser-binary budget up to the whole corpus,
/// so the size column answers "what does the last 32 KB actually buy".
fn sweep(o: &Opts) -> Result<(), Failure> {
    let zstd = measure::zstd_binary()
        .ok_or_else(|| Failure::Run("need the zstd binary on PATH (or $ZSTD)".into()))?;
    if o.samples.len() < 2 {
        return Err(Failure::Run("sweep needs at least two samples".into()));
    }

    let full = dictionary::build(vocabulary_of(o)?, &o.samples, None).map_err(io)?;
    let sizes: Vec<Option<usize>> = [4096usize, 8192, 16384, 32768]
        .into_iter()
        .filter(|s| *s < full.blob.len())
        .map(Some)
        .chain(std::iter::once(None))
        .collect();

    println!("held-out mean gain (%), by dictionary size and level");
    print!("{:>12}", "size");
    let levels = [3, 6, 9, 12, 19];
    for l in levels {
        print!("{:>9}", format!("L{l}"));
    }
    println!();

    let mut best = (f64::MIN, 0i32, None::<usize>);
    for size in &sizes {
        let label = match size {
            Some(n) => format!("{n}"),
            None => format!("{} (all)", full.blob.len()),
        };
        print!("{label:>12}");
        for level in levels {
            let rows =
                measure::leave_one_out(&zstd, level, o.vocabulary.as_deref(), &o.samples, *size)
                    .map_err(io)?;
            let gain = measure::mean_gain(&rows);
            print!("{gain:>8.1}%");
            if gain > best.0 {
                best = (gain, level, *size);
            }
        }
        println!();
    }

    let size = match best.2 {
        Some(n) => format!("{n} bytes"),
        None => format!("{} bytes (whole corpus)", full.blob.len()),
    };
    println!(
        "\nbest: level {} at {size}, {:.1}% mean gain",
        best.1, best.0
    );
    println!(
        "\nA sweep over {} documents overfits, and the best cell is not a\n\
         recommendation on its own — read the column, not the maximum. A level\n\
         that wins by a tenth of a point is noise in the corpus, not a finding.",
        o.samples.len()
    );
    Ok(())
}
