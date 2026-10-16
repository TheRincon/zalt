//! The text forms §9.1 puts macOS metadata in, as pure data.
//!
//! BSD file flags travel as libarchive's names (`SCHILY.fflags=uchg,hidden`),
//! ACLs as libarchive's NFSv4 text (`SCHILY.acl.ace=user:root:d::deny:0`). Both
//! are parsed and printed here, with no system calls, so the format layer
//! reads and writes them on any platform; only the `fs` module (feature
//! `native-fs`) turns them into flags and ACLs on real files.
//!
//! The spellings were taken from what bsdtar 3.7 actually writes, not from a
//! reading of the spec: `tests/oracle.rs` holds archives to that.

use crate::{Error, Result, format_err};

// ------------------------------------------------------------ file flags

/// BSD `st_flags` bits, in the numbering the index's `FFLAGS` field uses
/// (§7.2). macOS and FreeBSD agree on every bit listed here.
pub mod flag {
    pub const UF_NODUMP: u64 = 0x1;
    pub const UF_IMMUTABLE: u64 = 0x2;
    pub const UF_APPEND: u64 = 0x4;
    pub const UF_OPAQUE: u64 = 0x8;
    /// Transparent compression (macOS). §9.3: never recorded.
    pub const UF_COMPRESSED: u64 = 0x20;
    pub const UF_HIDDEN: u64 = 0x8000;
    pub const SF_ARCHIVED: u64 = 0x1_0000;
    pub const SF_IMMUTABLE: u64 = 0x2_0000;
    pub const SF_APPEND: u64 = 0x4_0000;
    pub const SF_NOUNLINK: u64 = 0x10_0000;
}

/// Each recordable flag: libarchive's name first, then the aliases its parser
/// accepts, in the order libarchive prints them.
///
/// Every other bit is either managed by the system (`UF_TRACKED`,
/// `SF_DATALESS`, `SF_FIRMLINK`, `SF_RESTRICTED`…), meaningless on another
/// machine, or — `UF_COMPRESSED` — forbidden by §9.3. None of them is
/// recorded.
const FLAG_NAMES: &[(u64, &[&str])] = &[
    (flag::SF_APPEND, &["sappnd", "sappend"]),
    (flag::SF_ARCHIVED, &["arch", "archived"]),
    (flag::SF_IMMUTABLE, &["schg", "schange", "simmutable"]),
    (flag::SF_NOUNLINK, &["sunlnk", "sunlink"]),
    (flag::UF_APPEND, &["uappnd", "uappend"]),
    (flag::UF_IMMUTABLE, &["uchg", "uchange", "uimmutable"]),
    // The odd one out: the flag is *named* for its set state, so `nodump`
    // sets it and `dump` clears it.
    (flag::UF_NODUMP, &["nodump"]),
    (flag::UF_OPAQUE, &["opaque"]),
    (flag::UF_HIDDEN, &["hidden", "uhidden"]),
];

/// The flags an archive records: every bit the name table can spell.
pub const RECORDED_FLAGS: u64 = {
    let mut bits = 0;
    let mut i = 0;
    while i < FLAG_NAMES.len() {
        bits |= FLAG_NAMES[i].0;
        i += 1;
    }
    bits
};

/// Flags that make a file refuse further changes, so they are set last.
pub const LOCKING_FLAGS: u64 =
    flag::UF_IMMUTABLE | flag::UF_APPEND | flag::SF_IMMUTABLE | flag::SF_APPEND;

/// `flags` as `SCHILY.fflags` text, e.g. `uchg,nodump,hidden`. Bits outside
/// [`RECORDED_FLAGS`] are dropped.
pub fn fflags_to_text(flags: u64) -> String {
    FLAG_NAMES
        .iter()
        .filter(|(bit, _)| flags & bit != 0)
        .map(|(_, names)| names[0])
        .collect::<Vec<_>>()
        .join(",")
}

/// Parse `SCHILY.fflags` text. Names this table does not know — a platform's
/// own flags, or a newer libarchive's — are ignored, as libarchive ignores
/// them: the flags are advisory metadata, and refusing the member over one
/// would lose its content for the sake of a bit.
pub fn fflags_from_text(text: &[u8]) -> u64 {
    let mut flags = 0u64;
    let words = text
        .split(|&b| b == b',' || b.is_ascii_whitespace())
        .filter(|w| !w.is_empty());
    for word in words {
        let word = core::str::from_utf8(word).unwrap_or("");
        if word == "dump" {
            flags &= !flag::UF_NODUMP;
            continue;
        }
        let (clear, name) = match word.strip_prefix("no") {
            Some(rest) if word != "nodump" => (true, rest),
            _ => (false, word),
        };
        if let Some((bit, _)) = FLAG_NAMES.iter().find(|(_, names)| names.contains(&name)) {
            if clear {
                flags &= !bit;
            } else {
                flags |= bit;
            }
        }
    }
    flags
}

// ------------------------------------------------------------------- ACLs

/// Who an ACL entry is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AceTag {
    User,
    Group,
    /// `owner@`: whoever owns the file.
    Owner,
    /// `group@`: the file's group.
    OwningGroup,
    /// `everyone@`.
    Everyone,
}

/// What an ACL entry does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AceType {
    Allow,
    Deny,
    Audit,
    Alarm,
}

/// The NFSv4 permission letters, in libarchive's printing order. Bit `i` of
/// [`Ace::perms`] is letter `i`:
///
/// `r` read data / list directory, `w` write data / add file, `x` execute /
/// search, `p` append data / add subdirectory, `D` delete child, `d` delete,
/// `a` read attributes, `A` write attributes, `R` read extended attributes,
/// `W` write extended attributes, `c` read ACL, `C` write ACL, `o` change
/// owner, `s` synchronize.
pub const PERM_LETTERS: &[u8; 14] = b"rwxpDdaARWcCos";

/// The NFSv4 inheritance letters, in libarchive's printing order. Bit `i` of
/// [`Ace::flags`] is letter `i`:
///
/// `f` file inherit, `d` directory inherit, `i` inherit only, `n` no
/// propagate, `S` successful access, `F` failed access, `I` inherited.
pub const FLAG_LETTERS: &[u8; 7] = b"fdinSFI";

/// One NFSv4 ACL entry, as `SCHILY.acl.ace` spells it:
/// `tag[:qualifier]:perms:flags:type[:id]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ace {
    pub tag: AceTag,
    /// The user or group name; empty for `owner@`, `group@` and `everyone@`.
    pub qualifier: Vec<u8>,
    /// Bit `i` is [`PERM_LETTERS`]`[i]`.
    pub perms: u16,
    /// Bit `i` is [`FLAG_LETTERS`]`[i]`.
    pub flags: u8,
    pub kind: AceType,
    /// The numeric uid or gid, for a reader with no such name.
    pub id: Option<u64>,
}

impl Ace {
    /// Whether this is one of the three entries that only restate the mode
    /// bits. libarchive writes them; macOS cannot store them, and the mode
    /// already says what they say.
    pub fn is_trivial(&self) -> bool {
        matches!(
            self.tag,
            AceTag::Owner | AceTag::OwningGroup | AceTag::Everyone
        )
    }
}

fn letters(field: &[u8], alphabet: &[u8], what: &str) -> Result<u32> {
    let mut bits = 0u32;
    for &c in field {
        if c == b'-' {
            continue; // the long form pads absent letters with dashes
        }
        match alphabet.iter().position(|&a| a == c) {
            Some(i) => bits |= 1 << i,
            None => return format_err(format!("ACL {what} letter {:?}", c as char)),
        }
    }
    Ok(bits)
}

fn spell(bits: u32, alphabet: &[u8], out: &mut Vec<u8>) {
    for (i, &c) in alphabet.iter().enumerate() {
        if bits & (1 << i) != 0 {
            out.push(c);
        }
    }
}

/// Parse `SCHILY.acl.ace`: entries separated by `,` (or newlines, which
/// libarchive's long form uses).
pub fn parse_acl(text: &[u8]) -> Result<Vec<Ace>> {
    let bad = |why: &str| Error::Format(format!("SCHILY.acl.ace: {why}"));
    let mut out = Vec::new();
    for item in text.split(|&b| b == b',' || b == b'\n') {
        if item.is_empty() {
            continue;
        }
        let fields: Vec<&[u8]> = item.split(|&b| b == b':').collect();
        let (tag, rest) = match fields[0] {
            b"user" => (AceTag::User, &fields[1..]),
            b"group" => (AceTag::Group, &fields[1..]),
            b"owner@" => (AceTag::Owner, &fields[1..]),
            b"group@" => (AceTag::OwningGroup, &fields[1..]),
            b"everyone@" => (AceTag::Everyone, &fields[1..]),
            _ => return Err(bad("unknown tag")),
        };
        let named = matches!(tag, AceTag::User | AceTag::Group);
        let (qualifier, rest) = if named {
            let (q, r) = rest.split_first().ok_or_else(|| bad("no qualifier"))?;
            (q.to_vec(), r)
        } else {
            (Vec::new(), rest)
        };
        // perms, flags, type, and an optional id.
        if !(3..=4).contains(&rest.len()) {
            return Err(bad("wrong number of fields"));
        }
        let perms = letters(rest[0], PERM_LETTERS, "permission")? as u16;
        let flags = letters(rest[1], FLAG_LETTERS, "flag")? as u8;
        let kind = match rest[2] {
            b"allow" => AceType::Allow,
            b"deny" => AceType::Deny,
            b"audit" => AceType::Audit,
            b"alarm" => AceType::Alarm,
            _ => return Err(bad("unknown entry type")),
        };
        let id = match rest.get(3) {
            None | Some([]) => None,
            Some(digits) => Some(
                core::str::from_utf8(digits)
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| bad("id is not a number"))?,
            ),
        };
        if named && qualifier.is_empty() && id.is_none() {
            return Err(bad("a user or group entry names no one"));
        }
        out.push(Ace {
            tag,
            qualifier,
            perms,
            flags,
            kind,
            id,
        });
    }
    Ok(out)
}

/// Print entries as `SCHILY.acl.ace` text, in libarchive's compact form.
pub fn format_acl(entries: &[Ace]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(match e.tag {
            AceTag::User => b"user".as_slice(),
            AceTag::Group => b"group",
            AceTag::Owner => b"owner@",
            AceTag::OwningGroup => b"group@",
            AceTag::Everyone => b"everyone@",
        });
        if matches!(e.tag, AceTag::User | AceTag::Group) {
            out.push(b':');
            out.extend_from_slice(&e.qualifier);
        }
        out.push(b':');
        spell(u32::from(e.perms), PERM_LETTERS, &mut out);
        out.push(b':');
        spell(u32::from(e.flags), FLAG_LETTERS, &mut out);
        out.push(b':');
        out.extend_from_slice(match e.kind {
            AceType::Allow => b"allow".as_slice(),
            AceType::Deny => b"deny",
            AceType::Audit => b"audit",
            AceType::Alarm => b"alarm",
        });
        if let Some(id) = e.id {
            out.push(b':');
            out.extend_from_slice(id.to_string().as_bytes());
        }
    }
    out
}

// ----------------------------------------------------------------- xattrs

/// Transparent-compression payload. §9.3: **MUST NOT** be archived, and a
/// reader has no business setting it either.
pub const XATTR_DECMPFS: &[u8] = b"com.apple.decmpfs";
/// Gatekeeper's mark on downloaded files.
pub const XATTR_QUARANTINE: &[u8] = b"com.apple.quarantine";
/// The system's record of which process created a file: machine-local.
pub const XATTR_PROVENANCE: &[u8] = b"com.apple.provenance";

/// Whether an xattr name can be written as a `SCHILY.xattr.<name>` pax
/// keyword: non-empty, and free of the `=` that ends a keyword and the NUL
/// that ends a C string.
pub fn xattr_name_is_portable(name: &[u8]) -> bool {
    !name.is_empty() && !name.contains(&b'=') && !name.contains(&0)
}

// -------------------------------------------- LIBARCHIVE.xattr.* decoding

/// Decode `%XX` escapes, which `LIBARCHIVE.xattr.<name>` uses in names.
pub fn url_decode(s: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            let hex = core::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    Some(out)
}

/// Decode standard base64, with or without `=` padding — libarchive writes it
/// without.
pub fn base64_decode(s: &[u8]) -> Option<Vec<u8>> {
    let s = s.strip_suffix(b"==").or(s.strip_suffix(b"=")).unwrap_or(s);
    let value = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    if s.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.chunks(4) {
        let mut acc = 0u32;
        for &c in chunk {
            acc = (acc << 6) | value(c)?;
        }
        acc <<= 6 * (4 - chunk.len() as u32);
        let bytes = acc.to_be_bytes();
        out.extend_from_slice(&bytes[1..chunk.len()]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fflags_print_as_bsdtar_prints_them() {
        // From `tar --fflags` on a file with `chflags uchg,hidden,nodump`.
        let bits = flag::UF_IMMUTABLE | flag::UF_HIDDEN | flag::UF_NODUMP;
        assert_eq!(fflags_to_text(bits), "uchg,nodump,hidden");
        assert_eq!(fflags_from_text(b"uchg,nodump,hidden"), bits);
    }

    #[test]
    fn fflags_parse_aliases_negations_and_ignore_the_unknown() {
        assert_eq!(
            fflags_from_text(b"uchange simmutable"),
            flag::UF_IMMUTABLE | flag::SF_IMMUTABLE
        );
        assert_eq!(fflags_from_text(b"uchg,nouchg"), 0);
        assert_eq!(fflags_from_text(b"nodump,dump"), 0);
        assert_eq!(fflags_from_text(b"hidden,frobnicate"), flag::UF_HIDDEN);
        assert_eq!(fflags_from_text(b""), 0);
    }

    #[test]
    fn every_recorded_flag_round_trips_and_compressed_never_does() {
        for &(bit, _) in FLAG_NAMES {
            assert_eq!(fflags_from_text(fflags_to_text(bit).as_bytes()), bit);
        }
        assert_eq!(fflags_to_text(flag::UF_COMPRESSED), "");
        assert_eq!(RECORDED_FLAGS & flag::UF_COMPRESSED, 0);
    }

    #[test]
    fn acl_text_round_trips_what_bsdtar_wrote() {
        // Verbatim from bsdtar 3.7.4 on macOS (two extended entries, then the
        // three trivial ones it derives from the mode).
        let text: &[u8] = b"user:4242:rW:f:allow:4242,group:staff:xp::allow:20,\
user:root:d::deny:0,owner@:rwpaARWcCos::allow,group@:raRcs::allow,everyone@:raRcs::allow";
        let acl = parse_acl(text).unwrap();
        assert_eq!(acl.len(), 6);
        assert_eq!(
            acl[0],
            Ace {
                tag: AceTag::User,
                qualifier: b"4242".to_vec(),
                perms: 0b1 | 0b10_0000_0000, // r, W
                flags: 0b1,                  // f
                kind: AceType::Allow,
                id: Some(4242),
            }
        );
        assert_eq!(acl[2].kind, AceType::Deny);
        assert!(acl[3].is_trivial() && !acl[2].is_trivial());
        assert_eq!(format_acl(&acl), text);
    }

    #[test]
    fn acl_long_form_with_dashes_parses_to_the_same_entry() {
        let long = parse_acl(b"user:dan:rw-p----------:-------:allow:501").unwrap();
        let short = parse_acl(b"user:dan:rwp::allow:501").unwrap();
        assert_eq!(long, short);
    }

    #[test]
    fn malformed_acl_text_is_refused() {
        for bad in [
            &b"wizard:dan:r::allow"[..],
            b"user:dan:r::permit",
            b"user:dan:rq::allow",
            b"user:dan:r::allow:x",
            b"user:dan:r:allow",
            b"user::r::allow",
            b"owner@:r::allow:0:extra",
        ] {
            assert!(parse_acl(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn libarchive_xattr_encodings_decode() {
        // From bsdtar: `LIBARCHIVE.xattr.com.zalt.bin=AP8Q` for 00 FF 10, and
        // `cGxhaW4gdmFsdWU` for "plain value".
        assert_eq!(base64_decode(b"AP8Q").unwrap(), [0x00, 0xff, 0x10]);
        assert_eq!(base64_decode(b"cGxhaW4gdmFsdWU").unwrap(), b"plain value");
        assert_eq!(base64_decode(b"cGxhaW4gdmFsdWU=").unwrap(), b"plain value");
        assert_eq!(base64_decode(b"").unwrap(), b"");
        assert!(base64_decode(b"A").is_none());
        assert!(base64_decode(b"A*==").is_none());
        assert_eq!(url_decode(b"user.a%3Db").unwrap(), b"user.a=b");
        assert!(url_decode(b"bad%4").is_none());
    }
}
