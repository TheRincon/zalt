//! Decompress a frame with `ruzstd`, the independent third-party decoder.
//!
//!   ruzstd_decompress <input.zst> <output>
//!
//! `ruzstd` is a dev-dependency, which examples may use. Having it reachable
//! from a shell is what lets `ci/interop.sh` check zalt's output against an
//! implementation that shares no code with either zalt or libzstd — so a
//! misreading of the format common to our encoder and our decoder cannot hide.

use std::io::Read;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: ruzstd_decompress <input.zst> <output>");
        std::process::exit(2);
    }
    let frame = std::fs::read(&args[0]).expect("readable input");
    let mut decoder = match ruzstd::StreamingDecoder::new(&frame[..]) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("ruzstd refused the frame: {e}");
            std::process::exit(1);
        }
    };
    let mut out = Vec::new();
    if let Err(e) = decoder.read_to_end(&mut out) {
        eprintln!("ruzstd failed while decoding: {e}");
        std::process::exit(1);
    }
    std::fs::write(&args[1], &out).expect("writable output");
}
