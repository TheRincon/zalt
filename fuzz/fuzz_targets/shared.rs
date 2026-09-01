// Shared setup for the fuzz targets.
//
// Included rather than imported: `cargo-fuzz` builds each target as its own
// binary crate, so a `mod` here is the simplest way to share without turning
// the fuzz package into a library.

use zalt_codec::params::DecodeLimits;

/// Bounds used by every target.
///
/// Deliberately **tighter than the library default**. A fuzzer that can make the
/// decoder allocate two gigabytes will spend its budget getting OOM-killed
/// instead of exploring the format, and every such report is noise rather than
/// a finding. The window cap here matches the browser policy in L2, which is
/// also the configuration that actually ships.
pub fn limits() -> DecodeLimits {
    DecodeLimits {
        max_window_log: 23,
        max_decoded_size: Some(16 << 20),
    }
}

/// Compression level derived from a fuzzer byte, across the useful range.
pub fn level_from(byte: u8) -> i32 {
    // 1..=9 covers every strategy the encoder switches between without paying
    // for the very slow high levels on every iteration.
    1 + i32::from(byte % 9)
}

/// A repeating pattern of chunk sizes driven by fuzzer bytes.
///
/// The consumer cycles this, so it does not need to cover the input — an
/// earlier version returned a fixed list sized to the frame and silently ran
/// out on large inputs, which looked like "the frame did not complete" rather
/// than a harness bug.
///
/// Sizes are biased small on purpose: boundary bugs live at one- to three-byte
/// chunks, not at two-hundred-byte ones. Zero is included because a zero-length
/// slice is legal input the decoder must handle by making no progress rather
/// than spinning.
pub fn chunk_schedule(seed: &[u8]) -> Vec<usize> {
    if seed.is_empty() {
        return vec![1];
    }
    seed.iter()
        .map(|byte| match byte % 8 {
            0 => 0,
            1 => 1,
            2 => 2,
            3 => 3,
            4 => 7,
            5 => 16,
            6 => 64,
            _ => 1 + usize::from(*byte),
        })
        .collect()
}
