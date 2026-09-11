//! Compress a file with zalt, for interoperability testing.
//!
//!   compress <input> <output> [stored|compressed] [checksum]
//!
//! There is no `zalt(1)` compress subcommand yet — the CLI is a decoder — so
//! this example is how the encoder is driven from a shell. `ci/interop.sh` uses
//! it to produce frames for other implementations to read.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: compress <input> <output> [stored|compressed] [checksum]");
        std::process::exit(2);
    }
    let strategy = match args.get(2).map(String::as_str) {
        Some("stored") | None => zalt_codec::Strategy::Stored,
        Some("compressed") => zalt_codec::Strategy::Compress,
        Some(other) => {
            eprintln!("unknown strategy: {other}");
            std::process::exit(2);
        }
    };
    let params = zalt_codec::EncodeParams {
        strategy,
        checksum: args.get(3).map(String::as_str) == Some("checksum"),
        ..Default::default()
    };

    let content = std::fs::read(&args[0]).expect("readable input");
    let frame = zalt_codec::compress(&content, &params).expect("encode");
    std::fs::write(&args[1], &frame).expect("writable output");
}
