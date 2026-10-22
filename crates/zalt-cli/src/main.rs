//! `zalt(1)` — compress, decompress, inspect and validate zstd frames from the
//! shell.
//!
//! # Why this exists
//!
//! Three jobs, in order of how much they matter.
//!
//! **It is the first real consumer of `zalt-embed`.** Every other caller so far
//! is a test. A CLI has to hold the streaming API the way a host does — feed
//! socket-sized chunks, drain output as it appears, never require the whole
//! response in memory — and an API that is awkward for this is an API that will
//! be awkward for a browser. That is worth finding out here.
//!
//! **It makes the policy surface visible.** The window cap, the memory budget
//! and the buffer recycler are the interesting parts of L2, and they are
//! otherwise invisible outside a test. Here they are flags, so they can be
//! reasoned about, demonstrated and misused deliberately.
//!
//! **It answers questions about files.** `info` walks frame and block structure
//! without decoding, which is what one actually wants when reading RFC 8878
//! next to a real `.zst`.
//!
//! # Streaming, not slurping
//!
//! `decompress` reads in `--in-buf` chunks and writes output as it is produced;
//! neither the compressed input nor the decoded output is ever held whole. That
//! is not an optimisation for a command-line tool — it is the property the
//! library claims, exercised by something other than a test.
//!
//! `info` is the exception and reads the whole file, which is stated where it
//! is implemented. So is `compress`, for a different reason: the encoder is
//! one-shot, and §10 is explicit that a streaming encoder should be written
//! when there is a caller to shape it.

#![forbid(unsafe_code)]

mod archive;
mod args;
mod inspect;

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, IsTerminal, Read, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use zalt_codec::decode::Status;
use zalt_embed::{Budget, Decoder, Recycler, RecyclerConfig, SharedSource};

use args::{Args, Command, USAGE, human};

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    // `zalt archive …` has its own arguments; see `archive.rs` for why it is a
    // separate parser rather than more flags on this one.
    if let Some((first, rest)) = argv.split_first()
        && matches!(first.as_str(), "archive" | "a")
    {
        return match archive::run(rest) {
            Ok(()) => ExitCode::SUCCESS,
            Err(archive::Outcome::Usage(archive::Usage(msg))) => {
                eprintln!("zalt archive: {msg}");
                eprintln!("try 'zalt archive help'");
                ExitCode::from(2)
            }
            Err(archive::Outcome::Failed(archive::Failed(msg))) => {
                eprintln!("zalt archive: {msg}");
                ExitCode::FAILURE
            }
        };
    }

    let args = match Args::parse(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("zalt: {e}");
            eprintln!("try 'zalt help'");
            return ExitCode::from(2);
        }
    };

    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage(msg)) => {
            eprintln!("zalt: {msg}");
            ExitCode::from(2)
        }
        Err(Failure::Input(msg)) => {
            eprintln!("zalt: {msg}");
            ExitCode::FAILURE
        }
    }
}

/// The two ways this program fails, kept apart because they mean different
/// things to a script: the command line was wrong, or an input was.
enum Failure {
    Usage(String),
    Input(String),
}

fn run(args: &Args) -> Result<(), Failure> {
    match args.command {
        Command::Help => {
            print!("{USAGE}");
            Ok(())
        }
        Command::Version => {
            println!("zalt {}", env!("CARGO_PKG_VERSION"));
            println!("backend: {}", zalt_embed::backend_name());
            Ok(())
        }
        Command::Compress => compress(args),
        Command::Decompress => decompress(args),
        Command::Info => info(args),
        Command::Check => check(args),
    }
}

// --------------------------------------------------------------- compress

/// Encode each input as one frame, to stdout or `--output`.
///
/// Several inputs to stdout become several frames back to back, which a
/// conformant decoder — this one included — reads as their contents
/// concatenated. That is the format's own rule, not a convention of this tool.
///
/// **Each input is read whole.** The encoder is one-shot: it chooses blocks
/// and matches over the entire input, and a streaming encoder is a state
/// machine §10 defers until something needs one. A browser never compresses
/// through this path; a person at a shell compressing a file can hold it.
fn compress(args: &Args) -> Result<(), Failure> {
    let params = args.encode_params();
    params
        .validate()
        .map_err(|e| Failure::Usage(format!("cannot encode with these settings: {e}")))?;

    let stdout = io::stdout();
    // zstd(1) refuses this too, for the same reason: a frame on a terminal is
    // noise that can leave the terminal in a strange state, and it is almost
    // always a forgotten redirect rather than an intention.
    if args.output.is_none() && stdout.is_terminal() {
        return Err(Failure::Usage(
            "refusing to write compressed data to a terminal; redirect it, or use -o FILE".into(),
        ));
    }
    let mut sink: Box<dyn Write> = match &args.output {
        Some(path) => Box::new(BufWriter::new(open_output(path, args.force)?)),
        None => Box::new(BufWriter::new(stdout.lock())),
    };

    let mut encode_one = |name: &str, content: &[u8]| -> Result<(), Failure> {
        let frame = zalt_embed::compress(content, &params)
            .map_err(|e| Failure::Input(format!("{name}: {e}")))?;
        sink.write_all(&frame)
            .map_err(|e| Failure::Input(format!("write failed: {e}")))?;
        report_compress(
            args,
            name,
            content.len() as u64,
            frame.len() as u64,
            &params,
        );
        Ok(())
    };

    if args.inputs.is_empty() {
        let mut content = Vec::new();
        io::stdin()
            .lock()
            .read_to_end(&mut content)
            .map_err(|e| Failure::Input(format!("<stdin>: {e}")))?;
        encode_one("<stdin>", &content)?;
    } else {
        for path in &args.inputs {
            let content =
                std::fs::read(path).map_err(|e| Failure::Input(format!("{path}: {e}")))?;
            encode_one(path, &content)?;
        }
    }
    sink.flush()
        .map_err(|e| Failure::Input(format!("write failed: {e}")))?;
    Ok(())
}

fn report_compress(
    args: &Args,
    name: &str,
    read: u64,
    written: u64,
    params: &zalt_codec::encode::EncodeParams,
) {
    if args.quiet {
        return;
    }
    let ratio = if written > 0 {
        read as f64 / written as f64
    } else {
        0.0
    };
    let how = if params.strategy == zalt_codec::encode::Strategy::Stored {
        "stored".to_string()
    } else {
        format!(
            "level {}",
            zalt_codec::encode::level::effective(params.level)
        )
    };
    eprintln!(
        "{name}: {} -> {} ({ratio:.2}x, {how}{})",
        human(read),
        human(written),
        if params.checksum { ", checksum" } else { "" },
    );
}

// ------------------------------------------------------------- decompress

fn decompress(args: &Args) -> Result<(), Failure> {
    let session = Session::new(args);

    // stdin and stdout are locked once, outside the loop. Rust re-acquires the
    // lock on every unlocked write otherwise, which turns a byte-at-a-time
    // writer into a lock-at-a-time writer.
    let stdout = io::stdout();
    let mut sink: Box<dyn Write> = match &args.output {
        Some(path) => {
            let file = open_output(path, args.force)?;
            Box::new(BufWriter::new(file))
        }
        None => Box::new(BufWriter::new(stdout.lock())),
    };

    if args.inputs.is_empty() {
        let stdin = io::stdin();
        let stats = session
            .stream(&mut stdin.lock(), &mut sink)
            .map_err(|e| Failure::Input(format!("<stdin>: {e}")))?;
        sink.flush()
            .map_err(|e| Failure::Input(format!("<stdout>: {e}")))?;
        report(args, "<stdin>", &stats);
        return Ok(());
    }

    for path in &args.inputs {
        let mut file = File::open(path).map_err(|e| Failure::Input(format!("{path}: {e}")))?;
        let stats = session
            .stream(&mut file, &mut sink)
            .map_err(|e| Failure::Input(format!("{path}: {e}")))?;
        report(args, path, &stats);
    }
    sink.flush()
        .map_err(|e| Failure::Input(format!("write failed: {e}")))?;
    Ok(())
}

/// What one input cost and produced.
#[derive(Debug, Default)]
struct Stats {
    read: u64,
    written: u64,
    frames: u64,
    peak_footprint: usize,
}

/// Everything shared across the inputs of one run.
///
/// The budget and the recycler must be built **once**, not per input: a budget
/// per file is not a ceiling on the process, and a recycler charging a budget
/// that no decoder can see is accounting that describes nothing. Sharing them
/// is also what makes `--recycle` do anything at all across several files —
/// the second decode reuses the buffers the first one grew.
struct Session<'a> {
    args: &'a Args,
    budget: Option<Budget>,
    source: Option<SharedSource>,
}

impl<'a> Session<'a> {
    fn new(args: &'a Args) -> Self {
        let budget = args.budget.map(Budget::new);
        let source: Option<SharedSource> = args.recycle.then(|| {
            let config = RecyclerConfig::default();
            let recycler = match &budget {
                Some(b) => Recycler::budgeted(config, b).unwrap_or_else(|| Recycler::new(config)),
                None => Recycler::new(config),
            };
            Arc::new(recycler) as SharedSource
        });
        Self {
            args,
            budget,
            source,
        }
    }

    fn decoder(&self) -> io::Result<Decoder> {
        match (&self.budget, &self.source) {
            (Some(budget), Some(source)) => {
                Decoder::with_source(self.args.policy, budget, source.clone())
                    .ok_or_else(|| failed("budget too small to admit a decoder"))
            }
            (Some(budget), None) => Decoder::budgeted(self.args.policy, budget)
                .ok_or_else(|| failed("budget too small to admit a decoder")),
            // Without a budget there is nothing to charge a recycler against,
            // so the decoder simply allocates. `--recycle` without `--budget`
            // is not an error; it just has nothing to account for.
            (None, _) => Ok(Decoder::new(self.args.policy)),
        }
    }

    /// Decode `input` into `output`, holding neither whole.
    ///
    /// This is the function the whole crate exists to write: it is what a
    /// host's response handler looks like, and if it is awkward here it will be
    /// awkward there.
    fn stream(&self, input: &mut impl Read, output: &mut impl Write) -> io::Result<Stats> {
        let args = self.args;
        let mut decoder = self.decoder()?;
        let mut stats = Stats::default();
        let mut chunk = vec![0u8; args.in_buf];
        // Input the decoder has seen but not consumed — a frame header split across
        // two reads lives here until the rest of it arrives.
        let mut carry: Vec<u8> = Vec::new();
        let mut decoded: Vec<u8> = Vec::new();

        loop {
            let n = input.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            stats.read += n as u64;
            carry.extend_from_slice(&chunk[..n]);

            let mut cursor = 0usize;
            loop {
                let progress = decoder
                    .decompress(&carry[cursor..], &mut decoded)
                    .map_err(embed_error)?;
                cursor += progress.consumed;

                if !decoded.is_empty() {
                    output.write_all(&decoded)?;
                    stats.written += decoded.len() as u64;
                    // The decoder owns the match history, so this buffer is free to
                    // be discarded — the property the window rewrite bought, and
                    // the reason a 500 MB response can go straight to a file.
                    decoded.clear();
                }
                stats.peak_footprint = stats.peak_footprint.max(decoder.footprint());

                if progress.status == Status::FrameComplete {
                    stats.frames += 1;
                }

                // No forward progress means the decoder is waiting on bytes that
                // have not arrived; without this the loop would spin.
                if progress.consumed == 0 || cursor >= carry.len() {
                    break;
                }
            }
            carry.drain(..cursor);
        }

        // The decoder's own state decides, not the last status it returned: a
        // stream that ends on a skippable frame — every ZALT-archive does, and
        // so does anything carrying a seekable-format seek table — finishes
        // with `NeedMoreInput`, and is complete. Judging by the last status
        // reported those as truncated.
        if !carry.is_empty() || (stats.read > 0 && !decoder.is_between_frames()) {
            return Err(failed("truncated input: the last frame does not end"));
        }
        Ok(stats)
    }
}

fn report(args: &Args, name: &str, stats: &Stats) {
    if args.quiet {
        return;
    }
    let ratio = if stats.read > 0 {
        stats.written as f64 / stats.read as f64
    } else {
        0.0
    };
    eprintln!(
        "{name}: {} -> {} ({ratio:.2}x, {} frame{}, peak {})",
        human(stats.read),
        human(stats.written),
        stats.frames,
        if stats.frames == 1 { "" } else { "s" },
        human(stats.peak_footprint as u64),
    );
    if !args.window_cap_is_default() {
        eprintln!(
            "  note: window ceiling raised to windowLog {}",
            args.policy.limits.max_window_log
        );
    }
}

// -------------------------------------------------------------------- info

fn info(args: &Args) -> Result<(), Failure> {
    if args.inputs.is_empty() {
        return Err(Failure::Usage(
            "info needs a file; it does not read stdin".into(),
        ));
    }
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    for path in &args.inputs {
        let data = std::fs::read(path).map_err(|e| Failure::Input(format!("{path}: {e}")))?;
        let report = inspect::walk(&data);
        let _ = write!(out, "{}", inspect::render(path, &data, &report));
    }
    let _ = out.flush();
    Ok(())
}

// ------------------------------------------------------------------- check

/// Decode every input and discard the output, reporting only the verdict.
///
/// Every input is attempted even after one fails, because the useful output of
/// a run over a corpus is the whole list of what broke, not the first entry.
fn check(args: &Args) -> Result<(), Failure> {
    if args.inputs.is_empty() {
        return Err(Failure::Usage("check needs at least one file".into()));
    }
    let session = Session::new(args);
    let mut failures = 0usize;

    for path in &args.inputs {
        let result = File::open(path).and_then(|mut f| session.stream(&mut f, &mut io::sink()));
        match result {
            Ok(stats) => {
                if !args.quiet {
                    println!(
                        "ok    {path}  {} -> {} ({} frame{})",
                        human(stats.read),
                        human(stats.written),
                        stats.frames,
                        if stats.frames == 1 { "" } else { "s" }
                    );
                }
            }
            Err(e) => {
                failures += 1;
                println!("FAIL  {path}  {e}");
            }
        }
    }

    if failures > 0 {
        return Err(Failure::Input(format!(
            "{failures} of {} input{} failed",
            args.inputs.len(),
            if args.inputs.len() == 1 { "" } else { "s" }
        )));
    }
    Ok(())
}

// ------------------------------------------------------------------ shared

fn open_output(path: &str, force: bool) -> Result<File, Failure> {
    // Refusing to clobber by default: `zalt -d x.zst -o x` with the argument
    // order muddled is a plausible way to lose the input, and an overwrite is
    // not recoverable.
    if !force && Path::new(path).exists() {
        return Err(Failure::Input(format!(
            "{path} exists; pass --force to overwrite"
        )));
    }
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|e| Failure::Input(format!("{path}: {e}")))
}

/// Carry a decode failure through `io::Result`, which is what the streaming
/// loop already deals in.
fn embed_error(e: zalt_embed::Error) -> io::Error {
    io::Error::other(e.to_string())
}

fn failed(msg: &str) -> io::Error {
    io::Error::other(msg.to_string())
}
