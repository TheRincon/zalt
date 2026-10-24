//! Recovery (§11.3) on arbitrary bytes: the input is a damaged file, so
//! nothing about it can be trusted — not the frames, not the tar headers, not
//! the sizes either declares.
//!
//! Beyond no panic, hang or unbounded allocation, one property: **whatever
//! recovery returns is a whole archive** — it opens by its index and passes
//! `verify`, which decodes every frame and checks the members tile the tar
//! stream — and its report adds up: kept and dropped bytes cover the input,
//! and no member is both kept and lost. A recovery that "succeeded" into an
//! unreadable file would be worse than one that failed.

#![no_main]

use libfuzzer_sys::fuzz_target;
use zalt_archive::read::Archive;
use zalt_archive::{FramePolicy, ReadLimits, RecoverOptions, WriterOptions, recover_with};

fuzz_target!(|data: &[u8]| {
    let options = WriterOptions {
        level: 1,
        policy: FramePolicy::default(),
        threads: 1,
        creator: None,
        hash: zalt_archive::Hash::default(),
        dictionary: None,
    };
    // Both modes: the search guesses where members start, so it is the one
    // most able to assemble something that does not hold together.
    let how = RecoverOptions {
        search: data.len() % 2 == 1,
    };
    let Ok((fixed, report)) = recover_with(data, Vec::new(), options, &how) else {
        return;
    };
    assert_eq!(report.kept + report.dropped, data.len() as u64);
    let archive = Archive::open(fixed.into_boxed_slice(), ReadLimits::default())
        .expect("a recovered archive opens");
    archive.verify().expect("a recovered archive verifies");
    assert_eq!(archive.entries().len() as u64, report.entries);
    for e in archive.entries() {
        assert!(
            !report.lost.contains(&e.path),
            "a member both kept and lost"
        );
    }
});
