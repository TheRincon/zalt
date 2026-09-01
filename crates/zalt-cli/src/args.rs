//! Argument parsing.
//!
//! Hand-written, because the workspace has no third-party dependencies and this
//! is not the place to acquire the first one. A CLI that takes four flags does
//! not need a derive macro and a dependency tree; what it needs is to reject
//! nonsense loudly, which is most of the code below.

use std::fmt;

use zalt_embed::Policy;
use zalt_embed::policy::UNTRUSTED_MAX_WINDOW_LOG;

/// What the user asked for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Decode frames from one input to one output.
    Decompress,
    /// Report frame structure without decoding.
    Info,
    /// Decode and discard, reporting only whether it worked.
    Check,
    /// Print the version and selected backend.
    Version,
    /// Print usage.
    Help,
}

/// A parsed command line.
#[derive(Debug)]
pub struct Args {
    /// What to do.
    pub command: Command,
    /// Inputs. Empty means stdin.
    pub inputs: Vec<String>,
    /// Explicit output path. `None` means stdout.
    pub output: Option<String>,
    /// Decode policy assembled from the limit flags.
    pub policy: Policy,
    /// Total decoder memory ceiling. `None` leaves the decode unbudgeted.
    pub budget: Option<u64>,
    /// Recycle decoder buffers through a bounded free list.
    pub recycle: bool,
    /// Bytes read from the input per call.
    pub in_buf: usize,
    /// Suppress the per-file summary on stderr.
    pub quiet: bool,
    /// Overwrite an existing output file.
    pub force: bool,
}

/// A usage error. Exits 2, never 1: a mistake in the invocation is not the
/// same event as a file that failed to decode, and a script driving this tool
/// wants to tell them apart.
#[derive(Debug)]
pub struct UsageError(pub String);

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

macro_rules! usage {
    ($($t:tt)*) => { return Err(UsageError(format!($($t)*))) };
}

impl Args {
    /// Parse `argv` minus the program name.
    pub fn parse(argv: &[String]) -> Result<Self, UsageError> {
        let mut args = Args {
            command: Command::Help,
            inputs: Vec::new(),
            output: None,
            policy: Policy::untrusted(),
            budget: None,
            recycle: false,
            // One socket read's worth. Small enough that the streaming path is
            // genuinely exercised rather than handed whole files.
            in_buf: 64 * 1024,
            quiet: false,
            force: false,
        };

        let mut it = argv.iter().peekable();
        let mut command_seen = false;
        // Everything after `--` is a path, even if it starts with a dash.
        let mut literal = false;

        // The first non-flag word is the command. `-d` is accepted as a
        // synonym for `decompress` because every other compressor spells it
        // that way and muscle memory is a real interface.
        while let Some(arg) = it.next() {
            if literal {
                args.inputs.push(arg.clone());
                continue;
            }
            match arg.as_str() {
                "--" => literal = true,
                "-h" | "--help" => {
                    args.command = Command::Help;
                    return Ok(args);
                }
                "-V" | "--version" => {
                    args.command = Command::Version;
                    return Ok(args);
                }
                "-q" | "--quiet" => args.quiet = true,
                "-f" | "--force" => args.force = true,
                "--recycle" => args.recycle = true,
                "--trusted" => {
                    // Keep any explicit limit already given: the flag selects a
                    // baseline, and a later `--max-window-log` narrows it.
                    let explicit = args.policy.limits.max_decoded_size;
                    args.policy = Policy::trusted();
                    args.policy.limits.max_decoded_size = explicit;
                }
                "-c" | "--stdout" => args.output = None,
                "-o" | "--output" => {
                    args.output = Some(take(&mut it, "--output")?);
                }
                "--max-window-log" => {
                    let raw = take(&mut it, "--max-window-log")?;
                    let n: u8 = raw.parse().map_err(|_| {
                        UsageError(format!("--max-window-log: not a number: {raw}"))
                    })?;
                    if !(10..=31).contains(&n) {
                        usage!("--max-window-log must be 10..=31, got {n}");
                    }
                    args.policy.limits.max_window_log = n;
                }
                "--max-decoded" => {
                    let raw = take(&mut it, "--max-decoded")?;
                    args.policy.limits.max_decoded_size = Some(parse_size(&raw)?);
                }
                "--budget" => {
                    let raw = take(&mut it, "--budget")?;
                    args.budget = Some(parse_size(&raw)?);
                }
                "--in-buf" => {
                    let raw = take(&mut it, "--in-buf")?;
                    let n = parse_size(&raw)?;
                    if n == 0 {
                        usage!("--in-buf must be at least 1");
                    }
                    args.in_buf = n.min(usize::MAX as u64) as usize;
                }
                "-d" if !command_seen => {
                    args.command = Command::Decompress;
                    command_seen = true;
                }
                other if other.starts_with('-') && other.len() > 1 => {
                    usage!("unknown option: {other}");
                }
                word if !command_seen => {
                    args.command = match word {
                        "decompress" | "d" => Command::Decompress,
                        "info" | "i" => Command::Info,
                        "check" => Command::Check,
                        "version" => Command::Version,
                        "help" => Command::Help,
                        other => usage!(
                            "unknown command: {other}\n\
                             expected one of: decompress, info, check, version, help"
                        ),
                    };
                    command_seen = true;
                }
                path => args.inputs.push(path.to_string()),
            }
        }

        if !command_seen {
            args.command = Command::Help;
        }
        args.validate()?;
        Ok(args)
    }

    fn validate(&self) -> Result<(), UsageError> {
        if self.command == Command::Decompress && self.inputs.len() > 1 && self.output.is_some() {
            usage!("--output takes one input; {} were given", self.inputs.len());
        }
        if self.command == Command::Check && self.output.is_some() {
            usage!("check produces no output; --output does not apply");
        }
        Ok(())
    }

    /// Whether the policy still carries the untrusted default window ceiling.
    ///
    /// Reported in the summary so a decode that succeeded only because the cap
    /// was raised says so.
    pub fn window_cap_is_default(&self) -> bool {
        self.policy.limits.max_window_log == UNTRUSTED_MAX_WINDOW_LOG
    }
}

fn take<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<String, UsageError> {
    match it.next() {
        Some(v) => Ok(v.clone()),
        None => Err(UsageError(format!("{flag} requires a value"))),
    }
}

/// Parse a byte count, with the suffixes people actually type.
///
/// Both `KiB` and `KB` mean 1024. Being pedantic about SI here would be
/// technically correct and would surprise every user, and the difference does
/// not matter for a memory ceiling.
pub fn parse_size(s: &str) -> Result<u64, UsageError> {
    let t = s.trim();
    if t.is_empty() {
        return Err(UsageError("empty size".into()));
    }
    let digits_end = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    if digits_end == 0 {
        return Err(UsageError(format!("not a size: {s}")));
    }
    let (num, suffix) = t.split_at(digits_end);
    let n: u64 = num
        .parse()
        .map_err(|_| UsageError(format!("size out of range: {s}")))?;

    let scale: u64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        other => return Err(UsageError(format!("unknown size suffix: {other:?}"))),
    };
    n.checked_mul(scale)
        .ok_or_else(|| UsageError(format!("size out of range: {s}")))
}

/// Render a byte count the way the summary lines want it.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub const USAGE: &str = "\
zalt — Zstandard (RFC 8878) decoder

USAGE
    zalt <command> [options] [file...]

COMMANDS
    decompress, d   decode frames to stdout or --output  (also: zalt -d)
    info, i         report frame and block structure without decoding
    check           decode and discard; exit non-zero if any input fails
    version         print the version and the selected backend
    help            this text

OPTIONS
    -o, --output PATH     write here instead of stdout (one input only)
    -c, --stdout          write to stdout (the default)
    -f, --force           overwrite an existing --output file
    -q, --quiet           suppress the per-file summary on stderr

    --max-window-log N    window ceiling, 10..=31. Default 23 (8 MiB), the
                          ceiling appropriate to untrusted network input.
    --max-decoded SIZE    refuse a frame that decodes to more than this
    --trusted             input we produced ourselves: lift the window
                          ceiling to the implementation maximum
    --budget SIZE         ceiling on total live decoder memory
    --recycle             reuse decoder buffers through a bounded free list
    --in-buf SIZE         input chunk size (default 64KiB)

    -h, --help            this text
    -V, --version         version

SIZES
    Plain bytes, or a suffix: 512, 64K, 8MiB, 1G. KB and KiB both mean 1024.

EXIT STATUS
    0   success
    1   an input was refused or failed to decode
    2   the command line was wrong
";
