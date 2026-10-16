//! The macOS metadata text forms (§9.1) and the pax records that carry them.
//!
//! Random bytes almost never get a tar header past its checksum, so
//! `archive_open` barely reaches these parsers; this target feeds them
//! directly. Properties:
//!
//! - `SCHILY.acl.ace`: whatever parses, printed and parsed again, is the
//!   same ACL — the printer never writes something the parser reads
//!   differently.
//! - `SCHILY.fflags`: any text yields only recordable bits, and those bits
//!   print and parse back to themselves.
//! - base64 and `%XX` decoding never panic.
//! - A member whose xattr value and ACL text are the fuzzer's bytes, behind a
//!   valid header, parses without panicking — and when it parses, hands back
//!   exactly those bytes.

#![no_main]

use libfuzzer_sys::fuzz_target;
use zalt_archive::meta;
use zalt_archive::tar::{self, Extras};
use zalt_archive::{Entry, Kind};

fuzz_target!(|data: &[u8]| {
    if let Ok(acl) = meta::parse_acl(data) {
        let printed = meta::format_acl(&acl);
        assert_eq!(
            meta::parse_acl(&printed).unwrap(),
            acl,
            "ACL print/parse disagree"
        );
    }

    let flags = meta::fflags_from_text(data);
    assert_eq!(flags & !meta::RECORDED_FLAGS, 0);
    assert_eq!(
        meta::fflags_from_text(meta::fflags_to_text(flags).as_bytes()),
        flags
    );

    let _ = meta::base64_decode(data);
    let _ = meta::url_decode(data);

    let extras = Extras {
        xattrs: vec![(b"user.fuzz".to_vec(), data.to_vec())],
        acl: Some(data.to_vec()),
    };
    let mut out = Vec::new();
    if tar::write_headers(&mut out, &Entry::new(Kind::File, "f"), &extras).is_ok() {
        if let Ok(m) = tar::parse_member(&out) {
            assert_eq!(m.xattrs, extras.xattrs);
            assert_eq!(m.acl, extras.acl);
        }
    }
});
