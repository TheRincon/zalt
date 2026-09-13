//! Benchmark harness.
//!
//! Two jobs at M0, before there is a decoder to measure:
//!
//! 1. **Baseline** — run the system `zstd` over a corpus directory to establish
//!    the ratio and throughput numbers zalt will be judged against. Recording
//!    these *now* means later comparisons are against a measurement rather than
//!    a memory.
//! 2. **Kernels** — time the backend seam's implementations against each other,
//!    so `zalt-neon` changes are justified by numbers from the first commit.
//!
//! The harness takes a corpus **directory** and has no built-in opinion about
//! content. Which files belong in a corpus is a recipe concern (L3), not a
//! benchmark concern — see `doc/ZALT-MAC.md` §5.
//!
//! Since M5 it also measures the *encoder*, which until then had never been
//! timed properly — see `doc/ZALT-MAC.md` §10. That mode differs from `decode`
//! in two ways worth knowing before reading its output: libzstd is linked in
//! rather than driven through its CLI, so a 3 KB resource is not measuring a
//! fork and two pipes; and the rivals alternate batches inside one loop, so
//! interference lands on all of them instead of on whichever ran second.
//!
//! ```text
//! cargo run --release -p zalt-bench -- kernels
//! cargo run --release -p zalt-bench -- baseline <corpus-dir>
//! cargo run --release -p zalt-bench -- decode <corpus-dir>
//! cargo run --release -p zalt-bench -- encode <corpus-dir>
//! cargo run --release -p zalt-bench -- profile <file.zst> [seconds]
//! cargo run --release -p zalt-bench -- profile-encode <file> [seconds]
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use zalt_codec::{Backend, Scalar};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("kernels");

    println!("zalt-bench — backend: {}", zalt_embed::backend_name());
    println!();

    match mode {
        "kernels" => bench_kernels(),
        "baseline" => match args.get(2) {
            Some(dir) => bench_baseline(Path::new(dir)),
            None => {
                eprintln!("usage: zalt-bench baseline <corpus-dir>");
                std::process::exit(2);
            }
        },
        "profile" => match args.get(2) {
            Some(file) => profile_decode(
                Path::new(file),
                args.get(3).and_then(|s| s.parse().ok()).unwrap_or(20),
                args.iter().any(|a| a == "scalar"),
            ),
            None => {
                eprintln!("usage: zalt-bench profile <file.zst> [seconds] [scalar]");
                std::process::exit(2);
            }
        },
        "decode" => match args.get(2) {
            Some(dir) => bench_decode(Path::new(dir), args.get(3).and_then(|s| s.parse().ok())),
            None => {
                eprintln!("usage: zalt-bench decode <corpus-dir> [level]");
                std::process::exit(2);
            }
        },
        "encode" => match args.get(2) {
            Some(dir) => bench_encode(Path::new(dir)),
            None => {
                eprintln!("usage: zalt-bench encode <corpus-dir>");
                std::process::exit(2);
            }
        },
        "profile-encode" => match args.get(2) {
            Some(file) => profile_encode(
                Path::new(file),
                args.get(3).and_then(|s| s.parse().ok()).unwrap_or(20),
            ),
            None => {
                eprintln!("usage: zalt-bench profile-encode <file> [seconds]");
                std::process::exit(2);
            }
        },
        other => {
            eprintln!(
                "unknown mode {other:?}; expected 'kernels', 'baseline', 'decode', 'encode', \
                 'profile' or 'profile-encode'"
            );
            std::process::exit(2);
        }
    }
}

// ---------------------------------------------------------------- kernels

/// Match-copy shapes worth timing separately.
///
/// Offsets are split around 16 because that is where the NEON kernel changes
/// strategy, and short offsets are the case that motivates it: indentation runs
/// and repeated tags produce them constantly in structured text.
const SHAPES: &[(&str, usize, usize)] = &[
    ("offset=1   len=64   (byte smear)", 1, 64),
    ("offset=3   len=64   (short period)", 3, 64),
    ("offset=7   len=256  (short period, long run)", 7, 256),
    ("offset=15  len=256  (widest overlapping)", 15, 256),
    ("offset=16  len=256  (boundary)", 16, 256),
    ("offset=64  len=256  (non-overlapping)", 64, 256),
    ("offset=1024 len=4096 (bulk)", 1024, 4096),
];

fn bench_kernels() {
    println!("match-copy kernels");
    println!("{:-<78}", "");
    println!("{:<44} {:>12} {:>12}", "shape", "scalar GB/s", "neon GB/s");

    for &(name, offset, len) in SHAPES {
        let scalar = time_copy_match::<Scalar>(offset, len);
        #[cfg(target_arch = "aarch64")]
        let accel = time_copy_match::<zalt_neon::Neon>(offset, len);
        #[cfg(not(target_arch = "aarch64"))]
        let accel = f64::NAN;

        println!("{name:<44} {scalar:>12.2} {accel:>12.2}");
    }

    println!();
    println!("xxh64");
    println!("{:-<78}", "");
    for size in [1024usize, 64 * 1024, 1024 * 1024] {
        let data = vec![0xA5u8; size];
        let gbs = throughput(size, || {
            std::hint::black_box(zalt_codec::xxh64::xxh64(&data, 0));
        });
        println!("{:<44} {gbs:>12.2}", format!("{size} bytes"));
    }
}

fn time_copy_match<B: Backend>(offset: usize, len: usize) -> f64 {
    let prefix = offset.max(4096);
    let mut buf = vec![0u8; prefix + len + 64];
    for (i, b) in buf[..prefix].iter_mut().enumerate() {
        *b = i as u8;
    }
    throughput(len, || {
        B::copy_match(&mut buf, prefix, offset, len);
        std::hint::black_box(&buf);
    })
}

/// A repeated measurement, kept together with evidence of how much to trust it.
#[derive(Debug, Clone, Copy)]
struct Sample {
    /// Best observed throughput, GB/s.
    best: f64,
    /// Middle observed throughput, GB/s.
    median: f64,
    /// `(best - worst) / best`. How much the machine moved under the
    /// measurement.
    spread: f64,
}

impl Sample {
    /// Whether the spread is wide enough that a small difference between two
    /// samples means nothing.
    fn noisy(&self) -> bool {
        self.spread > 0.15
    }
}

/// Measure `op`, repeatedly, and report the distribution.
///
/// # Why the best sample and not the mean
///
/// The first version of this took one timing run per shape, and the end-to-end
/// decode numbers moved by a factor of two between runs — enough that the same
/// backend measured both 0.58x and 1.63x against the same rival. Every
/// optimisation decision made against that would have been made against noise.
///
/// Interference on a shared laptop is one-sided: another process, a migration
/// between P and E cores, a thermal step, or a page fault can only ever make a
/// sample *slower*. Nothing makes a sample spuriously fast. So the fastest of
/// several batches is the closest estimate of what the code costs, and the mean
/// is an average of the code plus however busy the machine happened to be.
///
/// `spread` is reported alongside so the noise is visible rather than hidden:
/// a wide spread means the number is a range, and a caller should say so rather
/// than print two decimal places of it.
fn measure(bytes_per_op: usize, mut op: impl FnMut()) -> Sample {
    /// Long enough that timer granularity and loop overhead are irrelevant,
    /// short enough that a batch is unlikely to span a scheduling event.
    const BATCH: Duration = Duration::from_millis(20);
    const WARMUP: Duration = Duration::from_millis(100);
    const BATCHES: usize = 9;

    // Calibrate: find how many iterations fill a batch, from one timed probe
    // rather than a fixed count. A fixed 1000-iteration warmup is instant for a
    // 64-byte copy and takes seconds for a 10 MB frame.
    let probe_start = Instant::now();
    op();
    let probe = probe_start.elapsed().max(Duration::from_nanos(1));
    let iters = ((BATCH.as_secs_f64() / probe.as_secs_f64()) as u64).clamp(1, 1 << 24);

    let warm = Instant::now();
    while warm.elapsed() < WARMUP {
        for _ in 0..iters {
            op();
        }
    }

    let mut rates = Vec::with_capacity(BATCHES);
    for _ in 0..BATCHES {
        let start = Instant::now();
        for _ in 0..iters {
            op();
        }
        let elapsed = start.elapsed().as_secs_f64().max(f64::MIN_POSITIVE);
        rates.push(bytes_per_op as f64 * iters as f64 / elapsed / 1e9);
    }

    rates.sort_by(|a, b| a.partial_cmp(b).expect("no NaN from a positive divide"));
    let best = rates[rates.len() - 1];
    let worst = rates[0];
    Sample {
        best,
        median: rates[rates.len() / 2],
        spread: if best > 0.0 {
            (best - worst) / best
        } else {
            0.0
        },
    }
}

/// Best-case throughput of `op` in GB/s of `bytes_per_op`.
fn throughput(bytes_per_op: usize, op: impl FnMut()) -> f64 {
    measure(bytes_per_op, op).best
}

/// Measure several implementations of the same work, **alternating batches**.
///
/// # Why this exists beside [`measure`]
///
/// [`measure`] runs one implementation to completion. Comparing two rivals that
/// way puts them in different windows of wall-clock time, and on this machine
/// that is not a detail: §10 records the same measurement of one file giving
/// 0.37 s and then 0.17 s in consecutive runs. Whatever moved between those two
/// runs — a thermal step, a migration from a P core to an E core, another
/// process waking up — lands entirely on whichever rival happened to be running,
/// and shows up as a difference between them.
///
/// Round-robining the batches does not make the machine quieter. It makes the
/// interference *shared*, so a drift that would otherwise read as a 2x win
/// instead moves both columns together and leaves the ratio where it was. The
/// ratio is what gets quoted, so the ratio is what has to be defended.
///
/// Each op is calibrated to its own iteration count, so a fast one and a slow
/// one still contribute batches of comparable duration; the returned samples are
/// per-op, in the order given.
fn measure_interleaved(bytes_per_op: usize, ops: &mut [&mut dyn FnMut()]) -> Vec<Sample> {
    const BATCH: Duration = Duration::from_millis(20);
    const WARMUP: Duration = Duration::from_millis(60);
    const ROUNDS: usize = 9;

    let mut iters = Vec::with_capacity(ops.len());
    for op in ops.iter_mut() {
        let probe_start = Instant::now();
        op();
        let probe = probe_start.elapsed().max(Duration::from_nanos(1));
        iters.push(((BATCH.as_secs_f64() / probe.as_secs_f64()) as u64).clamp(1, 1 << 24));
    }

    // Warm every op before timing any of them, so the first round does not
    // charge one rival for cold caches the others have already paid off.
    for (op, &n) in ops.iter_mut().zip(&iters) {
        let warm = Instant::now();
        while warm.elapsed() < WARMUP {
            for _ in 0..n {
                op();
            }
        }
    }

    let mut rates: Vec<Vec<f64>> = vec![Vec::with_capacity(ROUNDS); ops.len()];
    for _ in 0..ROUNDS {
        for (i, op) in ops.iter_mut().enumerate() {
            let n = iters[i];
            let start = Instant::now();
            for _ in 0..n {
                op();
            }
            let elapsed = start.elapsed().as_secs_f64().max(f64::MIN_POSITIVE);
            rates[i].push(bytes_per_op as f64 * n as f64 / elapsed / 1e9);
        }
    }

    rates
        .into_iter()
        .map(|mut r| {
            r.sort_by(|a, b| a.partial_cmp(b).expect("no NaN from a positive divide"));
            let best = r[r.len() - 1];
            let worst = r[0];
            Sample {
                best,
                median: r[r.len() / 2],
                spread: if best > 0.0 {
                    (best - worst) / best
                } else {
                    0.0
                },
            }
        })
        .collect()
}

// --------------------------------------------------------------- baseline

fn bench_baseline(dir: &Path) {
    let files = match collect_files(dir) {
        Ok(f) if !f.is_empty() => f,
        Ok(_) => {
            eprintln!("no files found under {}", dir.display());
            eprintln!();
            eprintln!("Populate a corpus first — see corpus/README.md. The harness");
            eprintln!("deliberately ships no sample data: a benchmark corpus that");
            eprintln!("does not reflect real traffic is worse than none, because it");
            eprintln!("produces numbers people trust.");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("cannot read {}: {e}", dir.display());
            std::process::exit(1);
        }
    };

    let Some(zstd) = find_zstd() else {
        eprintln!("system `zstd` not found on PATH; cannot establish a baseline");
        std::process::exit(1);
    };
    println!("reference: {}", zstd.display());
    print_version(&zstd);
    println!();

    println!("{:-<78}", "");
    println!(
        "{:<34} {:>10} {:>10} {:>8} {:>10}",
        "file", "bytes", "zstd -3", "ratio", "level 19"
    );
    println!("{:-<78}", "");

    let mut total_raw = 0u64;
    let mut total_l3 = 0u64;
    let mut total_l19 = 0u64;

    for path in &files {
        let Ok(data) = fs::read(path) else { continue };
        if data.is_empty() {
            continue;
        }
        let raw = data.len() as u64;
        let l3 = compressed_size(&zstd, path, 3);
        let l19 = compressed_size(&zstd, path, 19);

        total_raw += raw;
        total_l3 += l3.unwrap_or(raw);
        total_l19 += l19.unwrap_or(raw);

        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let name = truncate(&name, 34);
        match (l3, l19) {
            (Some(a), Some(b)) => println!(
                "{name:<34} {raw:>10} {a:>10} {:>8.2} {b:>10}",
                raw as f64 / a as f64
            ),
            _ => println!("{name:<34} {raw:>10} {:>10} {:>8} {:>10}", "-", "-", "-"),
        }
    }

    println!("{:-<78}", "");
    if total_raw > 0 {
        println!(
            "{:<34} {total_raw:>10} {total_l3:>10} {:>8.2} {total_l19:>10}",
            format!("TOTAL ({} files)", files.len()),
            total_raw as f64 / total_l3.max(1) as f64
        );
        println!();
        println!(
            "overall: level 3 {:.3}x, level 19 {:.3}x",
            total_raw as f64 / total_l3.max(1) as f64,
            total_raw as f64 / total_l19.max(1) as f64
        );
    }

    println!();
    println!("These are the numbers zalt is judged against. Record them with the");
    println!("corpus revision; a ratio without its corpus is not a measurement.");
}

fn collect_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == "README.md" {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

fn find_zstd() -> Option<PathBuf> {
    for candidate in [
        "/opt/homebrew/bin/zstd",
        "/usr/local/bin/zstd",
        "/usr/bin/zstd",
    ] {
        let p = PathBuf::from(candidate);
        if p.exists() {
            return Some(p);
        }
    }
    let out = Command::new("which").arg("zstd").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

fn print_version(zstd: &Path) {
    if let Ok(out) = Command::new(zstd).arg("--version").output() {
        print!("{}", String::from_utf8_lossy(&out.stdout));
    }
}

/// `--no-check` for the same reason as `compress_bytes`, which see. The four
/// bytes are noise in a ratio, but a baseline should describe the same frames
/// the decode benchmark actually decodes.
fn compressed_size(zstd: &Path, path: &Path, level: u32) -> Option<u64> {
    let out = Command::new(zstd)
        .arg(format!("-{level}"))
        .arg("--no-check")
        .arg("-c")
        .arg("-q")
        .arg(path)
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout.len() as u64)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("…{}", &s[s.len() - (max - 1)..])
    }
}

// ---------------------------------------------------------- machine state

/// What the machine was doing while a measurement was taken.
///
/// § "Measurement discipline" says a ratio without its corpus is not a
/// measurement. The same is true of a throughput without its machine: the same
/// binary on the same input measured 339 MB/s and 535 MB/s on this laptop, and
/// the difference was entirely the state recorded here. Printing it beside the
/// numbers is what stops a figure being copied into a commit message without
/// the caveat that belongs with it.
#[derive(Debug)]
struct MachineState {
    load_1min: f64,
    swap_used_mb: f64,
    swap_total_mb: f64,
}

impl MachineState {
    fn read() -> Option<Self> {
        // `sysctl` rather than a crate: the workspace takes dependencies only
        // where being wrong would be silent, and
        // this is two numbers.
        let load = sysctl("vm.loadavg")?;
        // "{ 2.11 2.45 2.60 }"
        let load_1min = load
            .split_whitespace()
            .nth(1)
            .and_then(|v| v.parse().ok())
            .unwrap_or(f64::NAN);

        let swap = sysctl("vm.swapusage")?;
        // "total = 12288.00M  used = 11152.69M  free = 1135.31M  (encrypted)"
        let field = |name: &str| -> f64 {
            swap.split_whitespace()
                .skip_while(|w| *w != name)
                .nth(2)
                .and_then(|v| v.trim_end_matches('M').parse().ok())
                .unwrap_or(f64::NAN)
        };

        Some(Self {
            load_1min,
            swap_used_mb: field("used"),
            swap_total_mb: field("total"),
        })
    }

    /// Whether conditions are good enough to trust a small difference.
    ///
    /// Both thresholds are judgement, not physics. Load above a couple of cores
    /// means something else is running; swap in use at all on a machine with
    /// free RAM means the memory compressor has been active, and a decode that
    /// allocates its output is then measuring page reclaim as much as entropy
    /// coding.
    ///
    /// The swap allowance needs an absolute floor as well as a share, because
    /// Darwin sizes the swap file *dynamically*: `swap_total_mb` is itself a
    /// measure of how much pressure the machine has already been under — about
    /// 1 GB after a reboot, 12 GB after a long uneasy session. A purely
    /// relative test therefore gets **stricter as the machine gets cleaner**,
    /// which is backwards. 56 MB of residual swap is 5.5% of a fresh 1 GB file
    /// and fails a check that 600 MB would pass on a 12 GB one — so the
    /// warning fired on every run of the quiet session it exists to certify,
    /// making it unfalsifiable at exactly the moment it matters.
    fn quiet(&self) -> bool {
        /// Below this the compressor has not done enough to move a decode,
        /// whatever share of the current swap file it happens to represent.
        const SWAP_FLOOR_MB: f64 = 256.0;

        let swap_allowance = SWAP_FLOOR_MB.max(0.05 * self.swap_total_mb);
        self.load_1min < 2.0 && self.swap_used_mb < swap_allowance
    }

    fn report(&self) {
        println!(
            "machine: load {:.2}, swap {:.0}/{:.0} MB",
            self.load_1min, self.swap_used_mb, self.swap_total_mb
        );
        if !self.quiet() {
            println!();
            println!("  WARNING: not a quiet machine. Interference is one-sided, so these");
            println!("  numbers are *lower bounds* — the code is at least this fast. Treat a");
            println!("  difference under ~10% as unmeasured, and compare two binaries");
            println!("  alternating rather than two runs at different times.");
        }
    }
}

fn sysctl(name: &str) -> Option<String> {
    let out = Command::new("sysctl").arg("-n").arg(name).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// ----------------------------------------------------------------- decode

/// Time zalt's own decoder against the reference, over a corpus.
///
/// # Why this mode exists before any NEON work
///
/// §5's rule is that nothing lands without a benchmark, and the kernel timings
/// alone cannot enforce it: a kernel that doubles in isolation moves the end-to-
/// end number by whatever fraction of decode time it occupied, which is a
/// question only this mode can answer. The M0 lesson — a hand-written bulk
/// wildcopy that *lost* 40% to the auto-vectorised scalar loop — was found by
/// measuring, and would have been invisible without something to measure.
///
/// # What is being timed
///
/// Decode only. Each file is compressed once, up front, with the reference
/// encoder; the compressed bytes are then decoded repeatedly from memory. No
/// file I/O, no compression and no allocation of the input is inside the timed
/// region.
///
/// Both backends are timed on identical input in the same process, so the
/// comparison is not across runs. The reference decoder is timed through its
/// CLI, which means its number carries process startup and pipe overhead and is
/// **not** comparable at small sizes — it is here as an order-of-magnitude
/// sanity check, not a verdict. Read the scalar/neon columns against each
/// other; read the zstd column with suspicion.
fn bench_decode(dir: &Path, level: Option<u32>) {
    let level = level.unwrap_or(3);

    let files = match collect_files(dir) {
        Ok(f) if !f.is_empty() => f,
        Ok(_) => {
            eprintln!("no files found under {}", dir.display());
            eprintln!();
            eprintln!("Populate a corpus first — see corpus/README.md. Pointing this");
            eprintln!("at any directory works: the harness has no opinion about");
            eprintln!("content, which is a recipe concern (§5).");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("cannot read {}: {e}", dir.display());
            std::process::exit(1);
        }
    };

    let Some(zstd) = find_zstd() else {
        eprintln!("system `zstd` not found on PATH; cannot build the inputs");
        std::process::exit(1);
    };

    println!("decode throughput — level {level}, MB/s of *decoded* output");
    println!("reference: {}", zstd.display());
    if let Some(state) = MachineState::read() {
        state.report();
    }
    println!();
    println!("{:-<104}", "");
    println!(
        "{:<24} {:>9} {:>9} {:>11} {:>11} {:>10} {:>9} {:>8}",
        "file", "raw", "comp", "scalar MB/s", "neon MB/s", "sink MB/s", "ruzstd", "speedup"
    );
    println!("{:-<104}", "");

    let mut cases = Vec::new();
    for path in &files {
        let Ok(raw) = fs::read(path) else { continue };
        if raw.len() < 64 {
            // Too small to time meaningfully: the measurement would be almost
            // entirely per-call overhead, and reporting it as throughput would
            // be reporting the harness rather than the decoder.
            continue;
        }
        let Some(compressed) = compress_bytes(&zstd, path, level) else {
            continue;
        };
        // Never benchmark something that does not decode correctly. A kernel
        // that produces wrong bytes fast is the failure mode this whole seam
        // is arranged to prevent, and it must not show up as a good number.
        match zalt_codec::decompress(&compressed, permissive_limits()) {
            Ok(out) if out == raw => {}
            Ok(_) => {
                eprintln!(
                    "SKIP {}: decoded output differs from the input",
                    path.display()
                );
                continue;
            }
            Err(e) => {
                eprintln!("SKIP {}: {e}", path.display());
                continue;
            }
        }
        cases.push((path.clone(), raw.len(), compressed));
    }

    if cases.is_empty() {
        eprintln!("nothing decodable in {}", dir.display());
        std::process::exit(1);
    }

    let (mut total_raw, mut total_comp) = (0u64, 0u64);
    let (mut sum_scalar_time, mut sum_neon_time) = (0f64, 0f64);
    let mut sum_sink_time = 0f64;
    let mut sum_ruzstd_time = 0f64;

    let mut any_noisy = false;

    for (path, raw_len, compressed) in &cases {
        let scalar = time_decode::<Scalar>(compressed, *raw_len);
        #[cfg(target_arch = "aarch64")]
        let accel = time_decode::<zalt_neon::Neon>(compressed, *raw_len);
        #[cfg(not(target_arch = "aarch64"))]
        let accel = Sample {
            best: f64::NAN,
            median: f64::NAN,
            spread: 0.0,
        };

        #[cfg(target_arch = "aarch64")]
        let sink = time_decode_sink::<zalt_neon::Neon>(compressed, *raw_len);
        #[cfg(not(target_arch = "aarch64"))]
        let sink = time_decode_sink::<Scalar>(compressed, *raw_len);

        total_raw += *raw_len as u64;
        total_comp += compressed.len() as u64;
        // Sum *time*, not throughput: an average of MB/s figures weights a
        // 64-byte file the same as a 10 MB one and means nothing.
        sum_scalar_time += *raw_len as f64 / (scalar.best * 1e6);
        if accel.best.is_finite() {
            sum_neon_time += *raw_len as f64 / (accel.best * 1e6);
        }
        if sink.best.is_finite() {
            sum_sink_time += *raw_len as f64 / (sink.best * 1e6);
        }
        let rz = time_decode_ruzstd(compressed, *raw_len);
        if rz.best.is_finite() {
            sum_ruzstd_time += *raw_len as f64 / (rz.best * 1e6);
        }

        // A ratio between two noisy samples is a rumour. Say so in the row
        // rather than printing two decimal places of it and letting a reader
        // act on the digits.
        let noisy = scalar.noisy() || accel.noisy();
        any_noisy |= noisy;

        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        println!(
            "{:<24} {raw_len:>9} {:>9} {:>11.1} {:>11.1} {:>10.1} {:>9.1} {:>7.2}x{}",
            truncate(&name, 24),
            compressed.len(),
            scalar.best,
            accel.best,
            sink.best,
            rz.best,
            accel.best / scalar.best,
            if noisy { "  ~noisy" } else { "" }
        );
    }

    println!("{:-<104}", "");
    let overall_scalar = total_raw as f64 / sum_scalar_time / 1e6;
    let overall_neon = total_raw as f64 / sum_neon_time / 1e6;
    let overall_sink = total_raw as f64 / sum_sink_time / 1e6;
    let overall_ruzstd = total_raw as f64 / sum_ruzstd_time / 1e6;
    println!(
        "{:<24} {total_raw:>9} {total_comp:>9} {overall_scalar:>11.1} {overall_neon:>11.1} {overall_sink:>10.1} {overall_ruzstd:>9.1} {:>7.2}x",
        format!("TOTAL ({} files)", cases.len()),
        overall_neon / overall_scalar
    );
    println!();
    println!(
        "corpus ratio {:.2}x over {} files",
        total_raw as f64 / total_comp.max(1) as f64,
        cases.len()
    );
    if any_noisy {
        println!();
        println!("~noisy: best and worst batch differed by >15%. The machine moved");
        println!("        under the measurement; treat those rows as a range, and");
        println!("        re-run on an idle system before believing a small delta.");
    }
    println!();
    println!("Record these with the corpus revision before changing a kernel.");
    println!("A speedup without the number it improved on is not a measurement.");
    // A stable, machine-readable line so a driver script never has to guess at
    // column positions in a table meant for humans.
    println!(
        "SUMMARY scalar={overall_scalar:.1} neon={overall_neon:.1} sink={overall_sink:.1} ruzstd={overall_ruzstd:.1}"
    );
}

/// Time one whole-frame decode, reported in MB/s of decoded output.
///
/// A fresh decoder per iteration on purpose: that is the shape of the target
/// workload — many small resources, each a new frame — so per-frame setup
/// belongs *inside* the measurement. Hoisting it out would flatter exactly the
/// cost that dominates a 4 KB response.
/// Time the same decode through a zero-copy [`Sink`], which is what a host
/// that can consume borrowed runs actually does.
///
/// The sink here only counts bytes. That is not cheating by measuring nothing:
/// the decoder still writes every byte to its window and still checksums it,
/// and what disappears is precisely the copy into a buffer the caller did not
/// need. A real sink writes to a socket or a parser and pays its own cost,
/// which is not zalt's to measure.
fn time_decode_sink<B: Backend>(compressed: &[u8], decoded_len: usize) -> Sample {
    let limits = permissive_limits();
    // One decoder, reset between iterations, rather than a fresh one each time.
    //
    // This is what a host with a decoder pool does (§6), and it is also what
    // makes the measurement about the decoder. Allocating the window and block
    // buffer per iteration measures the allocator, and on a machine under
    // memory pressure it measures the *kernel* — reclaiming pages, and on
    // Darwin the memory compressor. With 11 GB of swap in use the same binary
    // and input measured 339 MB/s and 535 MB/s, a spread larger than any
    // change this harness exists to detect.
    let mut decoder = zalt_codec::Decoder::<B>::new(limits);
    let s = measure(decoded_len, || {
        decoder.reset();
        let mut total = 0usize;
        let mut sink = zalt_codec::FnSink(|chunk: &[u8]| total += chunk.len());
        let mut cursor = 0usize;
        loop {
            let progress = decoder
                .decompress_into(&compressed[cursor..], &mut sink)
                .expect("verified decodable before timing");
            cursor += progress.consumed;
            if cursor >= compressed.len() {
                break;
            }
            if progress.consumed == 0 {
                break;
            }
        }
        std::hint::black_box(total);
    });
    Sample {
        best: s.best * 1000.0,
        median: s.median * 1000.0,
        spread: s.spread,
    }
}

/// Time the independent Rust decoder on the same frame.
///
/// Allocates its output per iteration, so this is comparable with the
/// `scalar`/`neon` columns — which do the same — and *not* with `sink`, which
/// reuses a decoder and writes nowhere. Reading it against `sink` flatters us
/// by exactly the allocator.
fn time_decode_ruzstd(compressed: &[u8], decoded_len: usize) -> Sample {
    use std::io::Read;
    let s = measure(decoded_len, || {
        let mut decoder =
            ruzstd::StreamingDecoder::new(compressed).expect("verified decodable before timing");
        let mut out = Vec::with_capacity(decoded_len);
        decoder
            .read_to_end(&mut out)
            .expect("verified before timing");
        std::hint::black_box(&out);
    });
    Sample {
        best: s.best * 1000.0,
        median: s.median * 1000.0,
        spread: s.spread,
    }
}

fn time_decode<B: Backend>(compressed: &[u8], decoded_len: usize) -> Sample {
    let limits = permissive_limits();
    // `measure` reports GB/s, which is the right unit for a kernel moving bytes
    // and the wrong one for whole-frame decode — entropy decoding lands in the
    // hundreds of MB/s, and a table of "0.1" tells nobody anything.
    let s = measure(decoded_len, || {
        let out = zalt_codec::decompress_with::<B>(compressed, limits)
            .expect("verified decodable before timing");
        std::hint::black_box(&out);
    });
    Sample {
        best: s.best * 1000.0,
        median: s.median * 1000.0,
        spread: s.spread,
    }
}

/// Bounds wide enough not to reject a corpus file.
///
/// A benchmark is not a security boundary: a browser's 8 MiB ceiling is policy
/// for untrusted input, and applying it here would silently drop the large
/// files from the corpus and quietly change what is being measured.
fn permissive_limits() -> zalt_codec::params::DecodeLimits {
    zalt_codec::params::DecodeLimits::default()
}

/// Compress with the reference encoder, **without a content checksum**.
///
/// # Why `--no-check`
///
/// The corpus is built by shelling out to the zstd *CLI*, and the CLI writes a
/// content checksum by default. The *library* does not — `ZSTD_c_checksumFlag`
/// defaults off — so frames produced server-side by application code, which is
/// what a browser actually receives, typically carry none. Taking the CLI's
/// default meant every measurement in `doc/ZALT-MAC.md` §10 charged zalt for an
/// XXH64 pass over the output that the deployed workload does not pay.
///
/// It is worth 8% on this corpus (measured by alternating two frames that
/// differ only in this flag), so the harness was reporting a decoder about 8%
/// slower than the one that will run in the browser, on the input it will run
/// on. The checksum path is not thereby untested — correctness is pinned by the
/// conformance corpus, and its *cost* is measurable on demand by pointing
/// `zalt-bench profile` at a checksummed frame.
///
/// The reference's own `zstd -b` benchmarks no-checksum frames for exactly the
/// same reason, so this also makes the two directly comparable rather than
/// requiring the 16% correction that comparison needed before.
fn compress_bytes(zstd: &Path, path: &Path, level: u32) -> Option<Vec<u8>> {
    let out = Command::new(zstd)
        .arg(format!("-{level}"))
        .arg("--no-check")
        .arg("-c")
        .arg("-q")
        .arg(path)
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

// ----------------------------------------------------------------- encode

/// Encoder parameters matching what `just encode-ratio` measures.
fn encode_params() -> zalt_codec::encode::EncodeParams {
    zalt_codec::encode::EncodeParams {
        strategy: zalt_codec::encode::Strategy::Compress,
        // The `--no-check` argument from `compress_bytes`, on the other side of
        // the wire: the library default is off, and charging the encoder for a
        // checksum the deployed workload does not ask for would flatter libzstd
        // by exactly one pass over the input.
        checksum: false,
        ..Default::default()
    }
}

/// Compress `input` with libzstd, in process.
fn zstd_compress(input: &[u8], level: i32) -> Vec<u8> {
    zstd::bulk::compress(input, level).expect("libzstd compresses anything")
}

/// Time zalt's encoder against libzstd, over a corpus.
///
/// # Why the gap ratio and not MB/s
///
/// An absolute throughput off this laptop is a number about the laptop. §10
/// records 0.37 s and 0.17 s for the same encode in consecutive runs, and the
/// `MachineState` warning exists because the same binary on the same input
/// measured 339 and 535 MB/s. What survives that is the *ratio* between two
/// implementations measured in the same window, which is why every rival here is
/// timed by [`measure_interleaved`] rather than in its own pass, and why the
/// `SUMMARY` line carries gaps rather than rates.
///
/// # Why two levels of the reference
///
/// zalt's output sits between `zstd -3` and `-9` (§10, M5), so neither level
/// alone is a fair rival: `-3` is faster and larger, `-9` is slower and smaller,
/// and quoting only one picks the flattering half. Both are timed, the sizes are
/// printed beside the rates, and a reader can see what the speed bought.
fn bench_encode(dir: &Path) {
    let files = match collect_files(dir) {
        Ok(f) if !f.is_empty() => f,
        Ok(_) => {
            eprintln!("no files found under {}", dir.display());
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("cannot read {}: {e}", dir.display());
            std::process::exit(1);
        }
    };

    let params = encode_params();

    println!("encode throughput — MB/s of *input*, interleaved against libzstd");
    println!(
        "reference: libzstd {} (in process)",
        zstd::zstd_safe::version_string()
    );
    if let Some(state) = MachineState::read() {
        state.report();
    }
    println!();
    println!("{:-<118}", "");
    println!(
        "{:<22} {:>9} {:>9} {:>9} {:>9} {:>10} {:>10} {:>10} {:>8} {:>8}",
        "file",
        "raw",
        "zalt",
        "zstd -3",
        "zstd -9",
        "zalt MB/s",
        "-3 MB/s",
        "-9 MB/s",
        "gap -3",
        "gap -9"
    );
    println!("{:-<118}", "");

    let mut cases = Vec::new();
    for path in &files {
        let Ok(raw) = fs::read(path) else { continue };
        if raw.len() < 1024 {
            // Below this the measurement is mostly per-call overhead, and
            // reporting it as throughput reports the harness.
            continue;
        }
        // Never benchmark an encoder whose output does not decode. Bytes
        // produced fast and read by nobody is the failure the whole differential
        // apparatus exists to prevent, and it must not show up as a good number.
        match zalt_codec::encode::compress(&raw, &params) {
            Ok(frame) => match zalt_codec::decompress(&frame, permissive_limits()) {
                Ok(out) if out == raw => cases.push((path.clone(), raw)),
                _ => {
                    eprintln!("SKIP {}: our own frame does not round trip", path.display());
                    continue;
                }
            },
            Err(e) => {
                eprintln!("SKIP {}: {e}", path.display());
                continue;
            }
        }
    }

    if cases.is_empty() {
        eprintln!("nothing encodable in {}", dir.display());
        std::process::exit(1);
    }

    let mut total_raw = 0u64;
    let (mut total_zalt, mut total_l3, mut total_l9) = (0u64, 0u64, 0u64);
    let (mut t_zalt, mut t_l3, mut t_l9) = (0f64, 0f64, 0f64);
    let mut any_noisy = false;

    for (path, raw) in &cases {
        let zalt_size = zalt_codec::encode::compress(raw, &params)
            .expect("verified encodable")
            .len();
        let l3_size = zstd_compress(raw, 3).len();
        let l9_size = zstd_compress(raw, 9).len();

        let samples = measure_interleaved(
            raw.len(),
            &mut [
                &mut || {
                    let f = zalt_codec::encode::compress(raw, &params).expect("verified encodable");
                    std::hint::black_box(&f);
                },
                &mut || drop(std::hint::black_box(zstd_compress(raw, 3))),
                &mut || drop(std::hint::black_box(zstd_compress(raw, 9))),
            ],
        );
        let (zalt, l3, l9) = (samples[0], samples[1], samples[2]);

        total_raw += raw.len() as u64;
        total_zalt += zalt_size as u64;
        total_l3 += l3_size as u64;
        total_l9 += l9_size as u64;
        // Sum time, not rates: averaging MB/s weights a 1 KB file like a 10 MB
        // one and means nothing.
        t_zalt += raw.len() as f64 / (zalt.best * 1e9);
        t_l3 += raw.len() as f64 / (l3.best * 1e9);
        t_l9 += raw.len() as f64 / (l9.best * 1e9);

        let noisy = zalt.noisy() || l3.noisy() || l9.noisy();
        any_noisy |= noisy;

        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        println!(
            "{:<22} {:>9} {zalt_size:>9} {l3_size:>9} {l9_size:>9} {:>10.1} {:>10.1} {:>10.1} {:>7.2}x {:>7.2}x{}",
            truncate(&name, 22),
            raw.len(),
            zalt.best * 1000.0,
            l3.best * 1000.0,
            l9.best * 1000.0,
            l3.best / zalt.best,
            l9.best / zalt.best,
            if noisy { "  ~noisy" } else { "" }
        );
    }

    println!("{:-<118}", "");
    let (r_zalt, r_l3, r_l9) = (
        total_raw as f64 / t_zalt / 1e6,
        total_raw as f64 / t_l3 / 1e6,
        total_raw as f64 / t_l9 / 1e6,
    );
    println!(
        "{:<22} {total_raw:>9} {total_zalt:>9} {total_l3:>9} {total_l9:>9} {r_zalt:>10.1} {r_l3:>10.1} {r_l9:>10.1} {:>7.2}x {:>7.2}x",
        format!("TOTAL ({})", cases.len()),
        r_l3 / r_zalt,
        r_l9 / r_zalt
    );
    println!();
    println!(
        "ratio: zalt {:.3}x, zstd -3 {:.3}x, zstd -9 {:.3}x",
        total_raw as f64 / total_zalt.max(1) as f64,
        total_raw as f64 / total_l3.max(1) as f64,
        total_raw as f64 / total_l9.max(1) as f64,
    );
    if any_noisy {
        println!();
        println!("~noisy: best and worst round differed by >15% for some rival. The");
        println!("        batches are interleaved, so a shared disturbance cancels in the");
        println!("        gap columns — but treat a small delta in a noisy row as unmeasured.");
    }
    println!();
    println!("A gap above 1.00x means the reference is faster by that factor.");
    println!(
        "SUMMARY encode gap_l3={:.2} gap_l9={:.2} ratio_zalt={:.3} ratio_l3={:.3} ratio_l9={:.3}",
        r_l3 / r_zalt,
        r_l9 / r_zalt,
        total_raw as f64 / total_zalt.max(1) as f64,
        total_raw as f64 / total_l3.max(1) as f64,
        total_raw as f64 / total_l9.max(1) as f64,
    );
}

// ---------------------------------------------------------------- profile

/// Decode one frame in a loop so a sampling profiler can attach.
///
/// # Why this is a shipped mode and not a scratch file
///
/// Every worthwhile change in M3 came from a profile, and none of them came
/// from the ranked list in `doc/ZALT-MAC.md` §5 that was written without one.
/// The largest — a wrapping match copy that never reached an accelerated kernel
/// — was **invisible** in the throughput table: NEON and scalar decoded at the
/// same speed, which looked like the kernels not helping and was actually the
/// kernels not running. Only attribution showed that, and only because both
/// backends could be profiled the same way in the same process.
///
/// So the harness is permanent. `just profile-decode` drives it with `sample`,
/// which ships with macOS; the `profile` recipe drives Instruments for the
/// cycle-level work when the question is finer than "which function".
///
/// The loop deliberately allocates a fresh output buffer per iteration, because
/// that is what `decompress` does for a caller and hiding it would profile a
/// function nobody calls. It shows up as time in `main`, and it is real.
fn profile_decode(file: &Path, seconds: u64, force_scalar: bool) {
    let compressed = match fs::read(file) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("cannot read {}: {e}", file.display());
            std::process::exit(1);
        }
    };
    let limits = permissive_limits();

    // Fail here rather than inside the timed loop, where the error would be
    // reported once per iteration and buried under the profiler's output.
    let decoded_len = match zalt_codec::decompress(&compressed, limits) {
        Ok(out) => out.len(),
        Err(e) => {
            eprintln!("{}: {e}", file.display());
            std::process::exit(1);
        }
    };

    let backend = if force_scalar {
        "scalar"
    } else {
        zalt_embed::backend_name()
    };
    eprintln!(
        "profiling {} — {} -> {} bytes, backend {backend}, {seconds}s",
        file.display(),
        compressed.len(),
        decoded_len,
    );
    eprintln!("attach with:  sample {} {seconds}", std::process::id());

    // Steady state, matching what the `sink` column measures: one decoder,
    // reset between frames, and no output buffer. Profiling the one-shot path
    // instead means a large share of the samples land in the allocator and, on
    // a machine under memory pressure, in the kernel — real costs, but not the
    // ones a kernel change moves.
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut iterations = 0u64;
    while Instant::now() < deadline {
        decode_once(&compressed, limits, force_scalar);
        iterations += 1;
    }

    let produced = decoded_len as u64 * iterations;
    eprintln!(
        "{iterations} iterations, {produced} bytes, ~{:.0} MB/s",
        produced as f64 / seconds as f64 / 1e6
    );
}

/// Encode one file in a loop so a sampling profiler can attach.
///
/// The encode counterpart of [`profile_decode`], and here for the same reason:
/// the throughput table says how far behind the reference we are and never which
/// part is behind. §10 records three inferences about the *decoder* — bounds
/// checks, double-symbol Huffman, prefetching — that were all wrong, and a
/// profile that answered it in one run.
///
/// Everything the encoder does per frame is inside the loop, including the
/// matcher's allocation. That is not overhead to be hoisted out: §8's workload
/// is many small resources, one frame each, so per-frame setup is the cost that
/// dominates and hiding it would profile a function nobody calls.
fn profile_encode(file: &Path, seconds: u64) {
    let raw = match fs::read(file) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("cannot read {}: {e}", file.display());
            std::process::exit(1);
        }
    };
    let params = encode_params();

    // Fail out here rather than once per iteration under the profiler.
    let size = match zalt_codec::encode::compress(&raw, &params) {
        Ok(f) => f.len(),
        Err(e) => {
            eprintln!("{}: {e}", file.display());
            std::process::exit(1);
        }
    };

    eprintln!(
        "profiling encode of {} — {} -> {size} bytes, {seconds}s",
        file.display(),
        raw.len(),
    );
    eprintln!("attach with:  sample {} {seconds}", std::process::id());

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut iterations = 0u64;
    while Instant::now() < deadline {
        let frame = zalt_codec::encode::compress(&raw, &params).expect("verified before the loop");
        std::hint::black_box(&frame);
        iterations += 1;
    }

    let consumed = raw.len() as u64 * iterations;
    eprintln!(
        "{iterations} iterations, {consumed} bytes in, ~{:.0} MB/s",
        consumed as f64 / seconds as f64 / 1e6
    );
}

/// One decode through the chosen backend.
///
/// `force_scalar` exists so both backends can be profiled from one binary: the
/// question "is the accelerated path even being taken" is answered by comparing
/// two profiles, and comparing across two builds introduces differences that
/// have nothing to do with the backend.
fn decode_once(compressed: &[u8], limits: zalt_codec::params::DecodeLimits, force_scalar: bool) {
    #[cfg(target_arch = "aarch64")]
    if !force_scalar {
        return drive::<zalt_neon::Neon>(compressed, limits);
    }
    let _ = force_scalar;
    drive::<Scalar>(compressed, limits);
}

/// Decode one frame through a fresh decoder into a counting sink.
///
/// The decoder is built per call rather than hoisted because the profiler runs
/// one frame per iteration over a 60 MB corpus; the construction is amortised
/// to nothing, and hoisting it would need the borrow to outlive the loop for
/// no measurable gain.
fn drive<B: Backend>(compressed: &[u8], limits: zalt_codec::params::DecodeLimits) {
    let mut decoder = zalt_codec::Decoder::<B>::new(limits);
    let mut total = 0usize;
    let mut sink = zalt_codec::FnSink(|chunk: &[u8]| total += chunk.len());
    let mut cursor = 0usize;
    loop {
        let progress = decoder
            .decompress_into(&compressed[cursor..], &mut sink)
            .expect("verified decodable before the loop");
        cursor += progress.consumed;
        if cursor >= compressed.len() || progress.consumed == 0 {
            break;
        }
    }
    std::hint::black_box(total);
}

#[cfg(test)]
mod machine_state_tests {
    use super::MachineState;

    fn at(load_1min: f64, swap_used_mb: f64, swap_total_mb: f64) -> MachineState {
        MachineState {
            load_1min,
            swap_used_mb,
            swap_total_mb,
        }
    }

    /// The case the threshold was wrong about, and the reason it is pinned:
    /// this is the state of the machine immediately after the reboot the
    /// measurement protocol asks for, and the old relative-only test called it
    /// noisy.
    #[test]
    fn residual_swap_in_a_fresh_boots_small_swap_file_is_still_quiet() {
        assert!(at(1.30, 56.5, 1024.0).quiet());
    }

    /// The same *share* of a swap file grown large by real pressure is not
    /// quiet — the floor raises the allowance, it must not lower it.
    #[test]
    fn the_floor_never_admits_more_than_the_share_on_a_large_swap_file() {
        assert!(!at(1.30, 700.0, 12288.0).quiet());
        assert!(at(1.30, 400.0, 12288.0).quiet());
    }

    /// Swap heavy enough to matter is noisy however big the file is.
    #[test]
    fn swap_past_the_floor_is_noisy_on_a_small_swap_file() {
        assert!(!at(1.30, 300.0, 1024.0).quiet());
    }

    #[test]
    fn load_alone_is_enough_to_make_a_machine_noisy() {
        assert!(!at(4.00, 0.0, 1024.0).quiet());
    }
}
