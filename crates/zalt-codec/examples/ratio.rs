//! Compressed size for each file named on the command line.
//!
//! `just encode-ratio` pairs this with the reference so the gap is reported
//! rather than a bare ratio — §10 is emphatic that a number without the thing
//! it is being compared against is not a measurement.
//!
//! `ZALT_BLOCK_SIZE` overrides the block size, which `just block-sweep` uses to
//! ask whether splitting a block buys anything at all before a splitter is
//! written to do it cleverly.
//!
//! Ratios are deterministic, so unlike throughput this needs no machine-state
//! caveat: the same input gives the same bytes on any machine.

fn main() {
    let block_size = std::env::var("ZALT_BLOCK_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(128 * 1024u32);

    let params = zalt_codec::EncodeParams {
        strategy: zalt_codec::Strategy::Compress,
        block_size,
        ..Default::default()
    };
    for path in std::env::args().skip(1) {
        let content = std::fs::read(&path).expect("readable");
        if content.is_empty() {
            continue;
        }
        let frame = zalt_codec::compress(&content, &params).expect("encode");
        println!("{}\t{}\t{}", path, content.len(), frame.len());
    }
}
