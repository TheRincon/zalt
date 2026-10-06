//! `zalt archive` — ZSTA archives from the shell.
//!
//! ```text
//! zalt archive create -o out.tar.zst [-19] [--threads N] [--no-xattrs] [--no-acls] [--no-fflags] PATH...
//! zalt archive list [-l] ARCHIVE
//! zalt archive extract ARCHIVE [-C DIR] [-f] [--keep-setid] [--same-owner] [--devices] [PATH...]
//! zalt archive verify ARCHIVE
//! zalt archive append ARCHIVE [-19] [--threads N] PATH...
//! zalt archive recover DAMAGED -o OUT
//! ```
//!
//! A sub-parser of its own rather than more flags on the top-level one: the
//! archive commands take different arguments from the codec commands, and a
//! flag that is meaningful to one and silently ignored by the other is exactly
//! what `args.rs` refuses.
//!
//! What it produces is a plain `.tar.zst` — `zstd -dc out.tar.zst | tar -x`
//! extracts it — that `list` and `extract` read by its index instead, decoding
//! only the frames an entry touches.

use std::fs::{File, OpenOptions};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use zalt_archive::fs::{
    CreateOptions, ExtractOptions, Report, add_tree_with, archive_quarantine, extract,
};
use zalt_archive::read::Archive;
use zalt_archive::{Entry, Kind, ReadLimits, Writer, WriterOptions};

pub const USAGE: &str = "\
zalt archive — ZSTA archives: a .tar.zst that is also randomly accessible

USAGE
    zalt archive create -o ARCHIVE [options] PATH...
    zalt archive list [-l] ARCHIVE
    zalt archive extract ARCHIVE [options] [PATH...]
    zalt archive verify ARCHIVE
    zalt archive append ARCHIVE [options] PATH...
    zalt archive recover DAMAGED -o OUT

    (short: zalt a c | t | x | v | r)

CREATE
    -o, --output PATH     the archive to write (required)
    -1 .. -19             compression level (default 3)
    --threads N           frames compressed in parallel (default: one per core)
    -f, --force           overwrite an existing archive
    --no-xattrs           leave out extended attributes
    --no-acls             leave out ACLs
    --no-fflags           leave out BSD file flags (hidden, uchg, ...)
    --keep-provenance     keep com.apple.provenance (left out by default:
                          it only means something on this machine)
    Paths are stored as given, minus a leading / — as tar stores them.

EXTRACT
    -C DIR                extract into DIR (default: the current directory)
    -f, --force           overwrite existing files
    --keep-setid          keep setuid/setgid/sticky bits (stripped by default)
    --same-owner          restore owner and group (normally needs root)
    --devices             create device nodes (normally needs root)
    --no-xattrs, --no-acls, --no-fflags
                          do not restore that metadata
    PATH...               only these members, and anything under them
    A quarantined archive passes its quarantine to what it extracts.

APPEND
    Adds PATH... after the archive's last member; takes CREATE's options
    except -o. Works on a copy that replaces the archive only once complete,
    so a failure leaves the archive as it was. A path already in the archive
    is refused: members cannot be replaced.

RECOVER
    -o, --output PATH     where to write the recovered archive (required)
    -f, --force           overwrite an existing output
    Rebuilds an archive that lost its index and seek table — a write cut
    short — or has damaged frames, keeping every member the damage did not
    touch, byte for byte. Lists the members it could not keep. The damaged
    file is only read.

COMMON
    -q, --quiet           no per-member output
    -h, --help            this text

Any stock tool reads the result too:  zstd -dc ARCHIVE | tar -x
";

/// Exit 2: the command line was wrong.
pub struct Usage(pub String);
/// Exit 1: an input or the filesystem failed.
pub struct Failed(pub String);

pub enum Outcome {
    Usage(Usage),
    Failed(Failed),
}

impl From<zalt_archive::Error> for Outcome {
    fn from(e: zalt_archive::Error) -> Self {
        Outcome::Failed(Failed(e.to_string()))
    }
}

macro_rules! usage {
    ($($t:tt)*) => { return Err(Outcome::Usage(Usage(format!($($t)*)))) };
}

#[derive(Default)]
struct Opts {
    output: Option<PathBuf>,
    dir: Option<PathBuf>,
    level: Option<i32>,
    threads: usize,
    force: bool,
    quiet: bool,
    long: bool,
    keep_setid: bool,
    same_owner: bool,
    devices: bool,
    no_xattrs: bool,
    no_acls: bool,
    no_fflags: bool,
    keep_provenance: bool,
    paths: Vec<String>,
}

fn level_flag(arg: &str) -> Option<&str> {
    let digits = arg.strip_prefix('-')?;
    ((1..=2).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit()))
        .then_some(digits)
}

fn parse(argv: &[String], allowed: &[&str]) -> Result<Opts, Outcome> {
    let mut o = Opts::default();
    let mut it = argv.iter();
    let mut literal = false;
    let take = |it: &mut std::slice::Iter<'_, String>, flag: &str| -> Result<String, Outcome> {
        it.next()
            .cloned()
            .ok_or_else(|| Outcome::Usage(Usage(format!("{flag} requires a value"))))
    };
    while let Some(arg) = it.next() {
        if literal || !arg.starts_with('-') || arg == "-" {
            o.paths.push(arg.clone());
            continue;
        }
        let name = if level_flag(arg).is_some() {
            "-N"
        } else {
            arg.as_str()
        };
        let canonical = match name {
            "-o" | "--output" => "-o",
            "-f" | "--force" => "-f",
            "-q" | "--quiet" => "-q",
            "-l" | "--long" => "-l",
            other => other,
        };
        if canonical != "--" && !allowed.contains(&canonical) {
            usage!("{arg} does not apply here");
        }
        match canonical {
            "--" => literal = true,
            "-o" => o.output = Some(take(&mut it, arg)?.into()),
            "-C" => o.dir = Some(take(&mut it, arg)?.into()),
            "-f" => o.force = true,
            "-q" => o.quiet = true,
            "-l" => o.long = true,
            "--keep-setid" => o.keep_setid = true,
            "--same-owner" => o.same_owner = true,
            "--devices" => o.devices = true,
            "--no-xattrs" => o.no_xattrs = true,
            "--no-acls" => o.no_acls = true,
            "--no-fflags" => o.no_fflags = true,
            "--keep-provenance" => o.keep_provenance = true,
            "--threads" => {
                let raw = take(&mut it, arg)?;
                o.threads = raw.parse().map_err(|_| {
                    Outcome::Usage(Usage(format!("--threads: not a number: {raw}")))
                })?;
            }
            "-N" => {
                let n: i32 = level_flag(arg).unwrap_or_default().parse().unwrap_or(0);
                if !(1..=19).contains(&n) {
                    usage!("compression levels are 1..=19, got {n}");
                }
                o.level = Some(n);
            }
            _ => usage!("unknown option: {arg}"),
        }
    }
    Ok(o)
}

/// Run `zalt archive …`.
pub fn run(argv: &[String]) -> Result<(), Outcome> {
    let Some((sub, rest)) = argv.split_first() else {
        print!("{USAGE}");
        return Ok(());
    };
    match sub.as_str() {
        "create" | "c" => create(parse(
            rest,
            &[
                "-o",
                "-N",
                "--threads",
                "-f",
                "-q",
                "--no-xattrs",
                "--no-acls",
                "--no-fflags",
                "--keep-provenance",
            ],
        )?),
        "list" | "t" | "l" => list(parse(rest, &["-l", "-q"])?),
        "extract" | "x" => extract_cmd(parse(
            rest,
            &[
                "-C",
                "-f",
                "-q",
                "--keep-setid",
                "--same-owner",
                "--devices",
                "--no-xattrs",
                "--no-acls",
                "--no-fflags",
            ],
        )?),
        "verify" | "v" => verify(parse(rest, &["-q"])?),
        "append" | "r" => append_cmd(parse(
            rest,
            &[
                "-N",
                "--threads",
                "-q",
                "--no-xattrs",
                "--no-acls",
                "--no-fflags",
                "--keep-provenance",
            ],
        )?),
        "recover" => recover_cmd(parse(rest, &["-o", "-f", "-q"])?),
        "help" | "-h" | "--help" => {
            print!("{USAGE}");
            Ok(())
        }
        other => usage!(
            "unknown archive command: {other}\n\
             expected one of: create, list, extract, verify, append, recover"
        ),
    }
}

fn create(o: Opts) -> Result<(), Outcome> {
    let Some(output) = o.output.clone() else {
        usage!("create needs -o ARCHIVE");
    };
    if o.paths.is_empty() {
        usage!("create needs at least one path to archive");
    }
    if !o.force && output.exists() {
        return Err(Outcome::Failed(Failed(format!(
            "{} exists; pass -f to overwrite",
            output.display()
        ))));
    }
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&output)
        .map_err(|e| Outcome::Failed(Failed(format!("{}: {e}", output.display()))))?;
    let options = WriterOptions {
        level: o.level.unwrap_or(zalt_codec::encode::level::LEVEL_DEFAULT),
        threads: o.threads,
        ..WriterOptions::default()
    };
    let mut writer = Writer::new(BufWriter::new(file), options)?;
    let quiet = o.quiet;
    let metadata = metadata_options(&o);
    for path in &o.paths {
        add_tree_with(&mut writer, Path::new(path), &metadata, &mut |warning| {
            if !quiet {
                eprintln!("zalt: {warning}");
            }
        })?;
    }
    let (_, summary) = writer.finish()?;
    if !quiet {
        eprintln!(
            "{}: {} members, {} of tar in {} ({:.2}x), {} data frames",
            output.display(),
            summary.entries,
            crate::args::human(summary.tar_stream_size),
            crate::args::human(summary.archive_size),
            summary.tar_stream_size as f64 / summary.archive_size.max(1) as f64,
            summary.data_frames,
        );
    }
    Ok(())
}

fn metadata_options(o: &Opts) -> CreateOptions {
    CreateOptions {
        xattrs: !o.no_xattrs,
        acls: !o.no_acls,
        fflags: !o.no_fflags,
        keep_provenance: o.keep_provenance,
    }
}

fn failed(what: impl std::fmt::Display, e: impl std::fmt::Display) -> Outcome {
    Outcome::Failed(Failed(format!("{what}: {e}")))
}

fn append_cmd(o: Opts) -> Result<(), Outcome> {
    let Some((archive, paths)) = o.paths.split_first() else {
        usage!("append needs an archive and at least one path to add");
    };
    if paths.is_empty() {
        usage!("append needs at least one path to add");
    }
    let archive = Path::new(archive);
    let name = archive
        .file_name()
        .ok_or_else(|| failed(archive.display(), "not a file name"))?;
    // §10: "Writers that need atomicity SHOULD append to a copy and rename it
    // into place." Same directory, so the rename cannot cross filesystems.
    let copy = archive.with_file_name(format!(
        ".{}.zalt-append-{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    std::fs::copy(archive, &copy).map_err(|e| failed(archive.display(), e))?;
    let result = (|| -> Result<zalt_archive::write::Summary, Outcome> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&copy)
            .map_err(|e| failed(copy.display(), e))?;
        let options = WriterOptions {
            level: o.level.unwrap_or(zalt_codec::encode::level::LEVEL_DEFAULT),
            threads: o.threads,
            ..WriterOptions::default()
        };
        let mut writer =
            zalt_archive::append(file, options).map_err(|e| failed(archive.display(), e))?;
        let metadata = metadata_options(&o);
        for path in paths {
            add_tree_with(&mut writer, Path::new(path), &metadata, &mut |warning| {
                if !o.quiet {
                    eprintln!("zalt: {warning}");
                }
            })?;
        }
        let (out, summary) = writer.finish()?;
        let file = out
            .into_inner()
            .map_err(|e| failed(copy.display(), e.into_error()))?;
        file.sync_all().map_err(|e| failed(copy.display(), e))?;
        Ok(summary)
    })();
    let summary = match result {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_file(&copy);
            return Err(e);
        }
    };
    std::fs::rename(&copy, archive).map_err(|e| {
        let _ = std::fs::remove_file(&copy);
        failed(archive.display(), e)
    })?;
    if !o.quiet {
        eprintln!(
            "{}: {} members now, {} data frames, {}",
            archive.display(),
            summary.entries,
            summary.data_frames,
            crate::args::human(summary.archive_size),
        );
    }
    Ok(())
}

fn recover_cmd(o: Opts) -> Result<(), Outcome> {
    let [damaged] = o.paths.as_slice() else {
        usage!(
            "recover needs exactly one damaged archive, got {}",
            o.paths.len()
        );
    };
    let Some(output) = o.output.clone() else {
        usage!("recover needs -o OUT: the damaged file is never changed");
    };
    let file = File::open(damaged).map_err(|e| failed(damaged, e))?;
    let intact = Archive::open(
        Box::new(file.try_clone().map_err(|e| failed(damaged, e))?),
        ReadLimits::default(),
    )
    .and_then(|a| a.verify());
    if intact.is_ok() {
        if !o.quiet {
            eprintln!("{damaged}: intact — every frame checked; nothing to recover");
        }
        return Ok(());
    }
    let out = OpenOptions::new()
        .write(true)
        .create(true)
        .create_new(!o.force)
        .truncate(true)
        .open(&output)
        .map_err(|e| failed(output.display(), e))?;
    let options = WriterOptions {
        level: o.level.unwrap_or(zalt_codec::encode::level::LEVEL_DEFAULT),
        ..WriterOptions::default()
    };
    let (out, report) = match zalt_archive::recover(&file, BufWriter::new(out), options) {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_file(&output);
            return Err(failed(damaged, e));
        }
    };
    out.into_inner()
        .map_err(|e| failed(output.display(), e.into_error()))?
        .sync_all()
        .map_err(|e| failed(output.display(), e))?;
    if !o.quiet {
        eprintln!(
            "{}: {} members recovered; {} of {} bytes kept as they were",
            output.display(),
            report.entries,
            report.kept,
            report.kept + report.dropped,
        );
        if report.damaged_frames > 0 {
            eprintln!("  {} damaged data frame(s) left out", report.damaged_frames);
        }
        if !report.lost.is_empty() {
            const SHOWN: usize = 10;
            eprintln!("  {} member(s) lost:", report.lost.len());
            for path in report.lost.iter().take(SHOWN) {
                eprintln!("    {}", String::from_utf8_lossy(path));
            }
            if report.lost.len() > SHOWN {
                eprintln!("    … and {} more", report.lost.len() - SHOWN);
            }
        }
        if report.resynced_by_index {
            eprintln!("  the old index was used to find members past the damage");
        }
        eprintln!(
            "  {}",
            if report.terminator_found {
                "the archive's end was found"
            } else {
                "the archive was cut short: it now ends at its last complete member"
            }
        );
        if let Some(why) = &report.stopped {
            eprintln!("  stopped early: {why}");
        }
    }
    Ok(())
}

fn open_archive(o: &Opts) -> Result<Archive<File>, Outcome> {
    let [path] = o.paths.as_slice() else {
        usage!("expected exactly one archive, got {}", o.paths.len());
    };
    let file = File::open(path).map_err(|e| Outcome::Failed(Failed(format!("{path}: {e}"))))?;
    Archive::open(Box::new(file), ReadLimits::default())
        .map_err(|e| Outcome::Failed(Failed(format!("{path}: {e}"))))
}

fn kind_char(k: Kind) -> char {
    match k {
        Kind::File | Kind::HardLink => '-',
        Kind::Directory => 'd',
        Kind::Symlink => 'l',
        Kind::CharDevice => 'c',
        Kind::BlockDevice => 'b',
        Kind::Fifo => 'p',
    }
}

fn describe(e: &Entry) -> String {
    match (&e.kind, &e.link) {
        (Kind::Symlink, Some(t)) => format!("{} -> {}", e.path_lossy(), String::from_utf8_lossy(t)),
        (Kind::HardLink, Some(t)) => {
            format!("{} link to {}", e.path_lossy(), String::from_utf8_lossy(t))
        }
        _ => e.path_lossy().into_owned(),
    }
}

fn list(o: Opts) -> Result<(), Outcome> {
    // An argument count `open_archive` checks; `-l` is the only option here.
    let archive = open_archive(&Opts {
        paths: o.paths.clone(),
        ..Opts::default()
    })?;
    for e in archive.entries() {
        if o.long {
            println!(
                "{}{:04o} {:>6} {:>6} {:>12} {:>11} {}",
                kind_char(e.kind),
                e.mode,
                e.uid,
                e.gid,
                e.size,
                e.mtime_sec,
                describe(e)
            );
        } else {
            println!("{}", e.path_lossy());
        }
    }
    Ok(())
}

fn extract_cmd(o: Opts) -> Result<(), Outcome> {
    let Some((archive_path, selection)) = o.paths.split_first() else {
        usage!("extract needs an archive");
    };
    let file = File::open(archive_path)
        .map_err(|e| Outcome::Failed(Failed(format!("{archive_path}: {e}"))))?;
    let quarantine = archive_quarantine(&file);
    let archive = Archive::open(Box::new(file), ReadLimits::default())
        .map_err(|e| Outcome::Failed(Failed(format!("{archive_path}: {e}"))))?;

    // A member is selected if it is a named path or lies under one.
    let wanted: Vec<Vec<u8>> = selection
        .iter()
        .map(|p| p.trim_end_matches('/').as_bytes().to_vec())
        .collect();
    for w in &wanted {
        let present = archive
            .entries()
            .iter()
            .any(|e| e.path == *w || (e.path.starts_with(w) && e.path.get(w.len()) == Some(&b'/')));
        if !present {
            return Err(Outcome::Failed(Failed(format!(
                "{}: not in the archive",
                String::from_utf8_lossy(w)
            ))));
        }
    }
    let select = |e: &Entry| {
        wanted.is_empty()
            || wanted.iter().any(|w| {
                e.path == *w || (e.path.starts_with(w) && e.path.get(w.len()) == Some(&b'/'))
            })
    };
    let options = ExtractOptions {
        overwrite: o.force,
        keep_setid: o.keep_setid,
        restore_owner: o.same_owner,
        devices: o.devices,
        xattrs: !o.no_xattrs,
        acls: !o.no_acls,
        fflags: !o.no_fflags,
        quarantine,
        ..ExtractOptions::default()
    };
    let dest = o.dir.unwrap_or_else(|| PathBuf::from("."));
    let quiet = o.quiet;
    let summary = extract(
        &archive,
        &dest,
        &select,
        &options,
        &mut |e, what| match what {
            Report::Skipped(why) => eprintln!("zalt: {}: skipped — {why}", e.path_lossy()),
            Report::Warning(why) => eprintln!("zalt: {}: {why}", e.path_lossy()),
            Report::Extracted if !quiet => eprintln!("{}", describe(e)),
            Report::Extracted => {}
        },
    )?;
    if !o.quiet {
        eprintln!(
            "{} members, {}{}",
            summary.entries,
            crate::args::human(summary.bytes),
            if summary.skipped > 0 {
                format!(", {} skipped", summary.skipped)
            } else {
                String::new()
            },
        );
    }
    if summary.warnings > 0 {
        eprintln!(
            "zalt: {} metadata warning(s); the files themselves are complete",
            summary.warnings
        );
    }
    Ok(())
}

fn verify(o: Opts) -> Result<(), Outcome> {
    let archive = open_archive(&o)?;
    archive.verify()?;
    if !o.quiet {
        let data = archive.frames().iter().filter(|f| f.is_data()).count();
        eprintln!(
            "{}: ok — {} members, {} data frames, {} of tar stream, every frame checked",
            o.paths[0],
            archive.entries().len(),
            data,
            crate::args::human(archive.tar_stream_size())
        );
    }
    Ok(())
}
