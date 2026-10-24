//! Arbitrary bytes, opened as a ZALT-archive: never a panic, a hang or an
//! unbounded allocation — whatever the trailer, header or index claims.
//!
//! When an archive does open, everything a reader would then do is exercised
//! too: verify it end to end, and read every member. Each may fail; none may
//! crash. The limits are tight so a claim of millions of entries or frames is
//! refused at parse time, which is the behaviour under test, rather than
//! honoured slowly.

#![no_main]

use libfuzzer_sys::fuzz_target;
use zalt_archive::ReadLimits;
use zalt_archive::index::IndexLimits;
use zalt_archive::read::Archive;

fuzz_target!(|data: &[u8]| {
    let limits = ReadLimits {
        index: IndexLimits {
            max_entries: 4096,
            max_body: 1 << 20,
            max_path: 1024,
            max_xattrs_per_entry: 64,
        },
        max_frames: 4096,
        threads: 1,
        max_header_bytes: 1 << 20,
        // Small, so verifying and reading evict and refill the frame cache
        // rather than holding a whole fuzzed archive in it.
        cache_bytes: 64 << 10,
        // Small too: a fuzzed Dictionary frame is parsed whole before use.
        max_dictionary_bytes: 1 << 20,
    };
    let Ok(archive) = Archive::open(data.to_vec().into_boxed_slice(), limits) else {
        return;
    };
    let _ = archive.verify();
    for e in archive.entries() {
        let _ = archive.entry_bytes(e);
    }
});
