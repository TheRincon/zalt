//! Hostile-input robustness: arbitrary bytes must never panic, hang, or
//! allocate without bound.
//!
//! No oracle here, and none needed. The only assertion is the implicit one
//! libFuzzer makes: the process survives. Every byte this decoder parses in
//! production arrives from the open internet, so "produces the wrong answer" is
//! a correctness bug while "panics" is a denial of service and "reads out of
//! bounds" is a security incident.
//!
//! Run with `-rss_limit_mb` and a timeout so unbounded allocation and
//! non-termination are reported rather than silently tolerated; the justfile
//! recipe sets both.

#![no_main]

use libfuzzer_sys::fuzz_target;

include!("shared.rs");

fuzz_target!(|data: &[u8]| {
    // Errors are the expected outcome for nearly all input and are not
    // interesting. What matters is that control returns at all.
    let _ = zalt_codec::decompress(data, limits());
});
