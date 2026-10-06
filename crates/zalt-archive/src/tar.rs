//! The tar layer: POSIX.1-2001 pax over ustar (spec §5.1, §9).
//!
//! Every member is a ustar header, preceded by a pax extended header (`x`)
//! exactly when §9.1 says a value does not fit ustar: a path or link target
//! that is too long or not ASCII, a size of 8 GiB or more, an id beyond the
//! octal field, a time before 1970 or past the field, or sub-second time.
//!
//! The parser exists for the reader's side of §7.5 — the tar stream is
//! authoritative, the index a cache — and of §13, which applies its path rules
//! to tar headers as well as to the index. It reads what this writer writes
//! and the pax and ustar forms any conformant writer produces; GNU's
//! non-POSIX long-name members (`L`, `K`) and global headers (`g`, which §5.3
//! discourages) are refused as unsupported rather than half-read.

use crate::index::{Entry, Kind, validate_path};
use crate::meta;
use crate::{Error, Result, format_err};

/// The metadata that lives only in the tar stream: the index records xattr
/// names and sizes (§7.2) but never their values (§9.2), and ACLs not at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extras {
    /// Extended attributes, name and value, in the order to write them.
    pub xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    /// `SCHILY.acl.ace` text ([`crate::meta::format_acl`]).
    pub acl: Option<Vec<u8>>,
}

/// A tar block.
pub const BLOCK: usize = 512;

/// `n` rounded up to a whole number of blocks.
pub fn pad(n: u64) -> u64 {
    n.div_ceil(BLOCK as u64) * BLOCK as u64
}

const NAME: core::ops::Range<usize> = 0..100;
const MODE: core::ops::Range<usize> = 100..108;
const UID: core::ops::Range<usize> = 108..116;
const GID: core::ops::Range<usize> = 116..124;
const SIZE: core::ops::Range<usize> = 124..136;
const MTIME: core::ops::Range<usize> = 136..148;
const CHKSUM: core::ops::Range<usize> = 148..156;
const TYPEFLAG: usize = 156;
const LINKNAME: core::ops::Range<usize> = 157..257;
const MAGIC: core::ops::Range<usize> = 257..263;
const VERSION: core::ops::Range<usize> = 263..265;
const UNAME: core::ops::Range<usize> = 265..297;
const GNAME: core::ops::Range<usize> = 297..329;
const DEVMAJOR: core::ops::Range<usize> = 329..337;
const DEVMINOR: core::ops::Range<usize> = 337..345;
const PREFIX: core::ops::Range<usize> = 345..500;

/// Largest value an octal field of `width` bytes holds: `width - 1` digits and
/// a terminating NUL.
fn octal_max(width: usize) -> u64 {
    (1u64 << (3 * (width as u32 - 1))) - 1
}

fn put_octal(block: &mut [u8], field: core::ops::Range<usize>, value: u64) {
    let width = field.len();
    let digits = format!("{:0w$o}", value.min(octal_max(width)), w = width - 1);
    block[field.start..field.start + width - 1].copy_from_slice(digits.as_bytes());
    block[field.end - 1] = 0;
}

fn put_str(block: &mut [u8], field: core::ops::Range<usize>, value: &[u8]) {
    let n = value.len().min(field.len());
    block[field.start..field.start + n].copy_from_slice(&value[..n]);
}

/// One pax record: `"<len> <key>=<value>\n"`, where `<len>` counts itself.
fn pax_record(out: &mut Vec<u8>, key: &str, value: &[u8]) {
    pax_record_bytes(out, key.as_bytes(), value);
}

/// [`pax_record`] for a keyword that is not UTF-8 — an xattr name, which the
/// filesystem hands over as bytes.
fn pax_record_bytes(out: &mut Vec<u8>, key: &[u8], value: &[u8]) {
    let body = 1 + key.len() + 1 + value.len() + 1; // " key=value\n"
    let mut digits = 1;
    while (body + digits).to_string().len() != digits {
        digits += 1;
    }
    out.extend_from_slice((body + digits).to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(key);
    out.push(b'=');
    out.extend_from_slice(value);
    out.push(b'\n');
}

/// A pax decimal time: whole seconds, then nanoseconds when there are any.
/// Negative times are spelled as the real number they are, so -1 s plus
/// 0.5 s is `-0.5`, not `-1.5`.
fn pax_time(sec: i64, nsec: u32) -> String {
    if nsec == 0 {
        return sec.to_string();
    }
    let (sign, whole, frac) = if sec < 0 {
        ("-", (-(sec + 1)) as u64, 1_000_000_000 - nsec)
    } else {
        ("", sec as u64, nsec)
    };
    let frac = format!("{frac:09}");
    format!("{sign}{whole}.{}", frac.trim_end_matches('0'))
}

/// Split a path into ustar `prefix` and `name`, if it fits that way.
fn ustar_split(path: &[u8]) -> Option<(&[u8], &[u8])> {
    if path.len() <= NAME.len() {
        return Some((&[], path));
    }
    if path.len() > PREFIX.len() + 1 + NAME.len() {
        return None;
    }
    // The last `/` that leaves a name of at most 100 bytes and a prefix of at
    // most 155.
    (0..path.len())
        .rev()
        .filter(|&i| path[i] == b'/')
        .find(|&i| path.len() - i - 1 <= NAME.len() && i <= PREFIX.len() && i > 0)
        .map(|i| (&path[..i], &path[i + 1..]))
}

fn header_block(
    e: &Entry,
    typeflag: u8,
    size: u64,
    name: (&[u8], &[u8]),
    link: &[u8],
) -> [u8; BLOCK] {
    let mut b = [0u8; BLOCK];
    put_str(&mut b, NAME, name.1);
    put_str(&mut b, PREFIX, name.0);
    put_octal(&mut b, MODE, u64::from(e.mode & 0o7777));
    put_octal(&mut b, UID, e.uid);
    put_octal(&mut b, GID, e.gid);
    put_octal(&mut b, SIZE, size);
    put_octal(&mut b, MTIME, e.mtime_sec.max(0) as u64);
    b[TYPEFLAG] = typeflag;
    put_str(&mut b, LINKNAME, link);
    b[MAGIC].copy_from_slice(b"ustar\0");
    b[VERSION].copy_from_slice(b"00");
    if let Some((u, g)) = &e.names {
        put_str(&mut b, UNAME, u);
        put_str(&mut b, GNAME, g);
    }
    if let Some((major, minor)) = e.device {
        put_octal(&mut b, DEVMAJOR, major);
        put_octal(&mut b, DEVMINOR, minor);
    }
    // The checksum is computed with its own field as eight spaces.
    b[CHKSUM].fill(b' ');
    let sum: u64 = b.iter().map(|&x| u64::from(x)).sum();
    let digits = format!("{sum:06o}");
    b[CHKSUM.start..CHKSUM.start + 6].copy_from_slice(digits.as_bytes());
    b[CHKSUM.start + 6] = 0;
    b[CHKSUM.start + 7] = b' ';
    b
}

/// Append `e`'s header blocks — a pax header when needed, then ustar — and
/// return how many bytes they took. Content follows, written by the caller.
///
/// The xattrs written are `extras.xattrs`; `e.xattrs` is the index's summary
/// of them, and the caller keeps the two in agreement.
pub fn write_headers(out: &mut Vec<u8>, e: &Entry, extras: &Extras) -> Result<u64> {
    validate_path(&e.path)?;
    let start = out.len();
    let ascii = |b: &[u8]| b.is_ascii();
    let link = e.link.as_deref().unwrap_or(&[]);

    let mut records = Vec::new();
    let split = ustar_split(&e.path).filter(|_| ascii(&e.path));
    if split.is_none() {
        pax_record(&mut records, "path", &e.path);
    }
    let link_fits = link.len() <= LINKNAME.len() && ascii(link);
    if !link_fits {
        pax_record(&mut records, "linkpath", link);
    }
    if e.size > octal_max(SIZE.len()) {
        pax_record(&mut records, "size", e.size.to_string().as_bytes());
    }
    if e.uid > octal_max(UID.len()) {
        pax_record(&mut records, "uid", e.uid.to_string().as_bytes());
    }
    if e.gid > octal_max(GID.len()) {
        pax_record(&mut records, "gid", e.gid.to_string().as_bytes());
    }
    if e.mtime_nsec != 0 || e.mtime_sec < 0 || e.mtime_sec as u64 > octal_max(MTIME.len()) {
        pax_record(
            &mut records,
            "mtime",
            pax_time(e.mtime_sec, e.mtime_nsec).as_bytes(),
        );
    }
    if let Some((u, g)) = &e.names {
        if u.len() > UNAME.len() || !ascii(u) {
            pax_record(&mut records, "uname", u);
        }
        if g.len() > GNAME.len() || !ascii(g) {
            pax_record(&mut records, "gname", g);
        }
    }
    if let Some((major, minor)) = e.device {
        // No POSIX keyword exists for these; `SCHILY.dev*` is what star and
        // libarchive use when the octal field is too small.
        if major > octal_max(DEVMAJOR.len()) || minor > octal_max(DEVMINOR.len()) {
            pax_record(
                &mut records,
                "SCHILY.devmajor",
                major.to_string().as_bytes(),
            );
            pax_record(
                &mut records,
                "SCHILY.devminor",
                minor.to_string().as_bytes(),
            );
        }
    }
    if let Some((sec, nsec)) = e.btime {
        pax_record(
            &mut records,
            "LIBARCHIVE.creationtime",
            pax_time(sec, nsec).as_bytes(),
        );
    }
    if let Some(flags) = e.fflags {
        let text = meta::fflags_to_text(flags);
        if !text.is_empty() {
            pax_record(&mut records, "SCHILY.fflags", text.as_bytes());
        }
    }
    for (name, value) in &extras.xattrs {
        if !meta::xattr_name_is_portable(name) {
            return Err(Error::Refused(format!(
                "{}: xattr name {:?} cannot be a pax keyword",
                e.path_lossy(),
                String::from_utf8_lossy(name)
            )));
        }
        let key = [b"SCHILY.xattr.".as_slice(), name].concat();
        pax_record_bytes(&mut records, &key, value);
    }
    if let Some(acl) = &extras.acl {
        pax_record(&mut records, "SCHILY.acl.ace", acl);
    }

    if !records.is_empty() {
        // The pax header's own name is informative; this is bsdtar's form.
        let base = e.path.rsplit(|&b| b == b'/').next().unwrap_or(&e.path);
        let mut name = b"PaxHeader/".to_vec();
        name.extend(
            base.iter()
                .copied()
                .filter(u8::is_ascii)
                .take(NAME.len() - 10),
        );
        let pax = header_block(e, b'x', records.len() as u64, (&[], &name), &[]);
        out.extend_from_slice(&pax);
        out.extend_from_slice(&records);
        out.resize(start + BLOCK + pad(records.len() as u64) as usize, 0);
    }

    // The ustar fields hold what fits; the pax records above override them
    // for every reader that knows pax, and are what a stock reader falls back
    // from.
    let (prefix, name) = split.unwrap_or((&[], &e.path[..e.path.len().min(NAME.len())]));
    let link_field = if link_fits {
        link
    } else {
        &link[..link.len().min(LINKNAME.len())]
    };
    let size = if e.kind.has_content() { e.size } else { 0 };
    out.extend_from_slice(&header_block(
        e,
        e.kind.typeflag(),
        size,
        (prefix, name),
        link_field,
    ));
    Ok((out.len() - start) as u64)
}

/// A member as its tar headers describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub kind: Kind,
    pub path: Vec<u8>,
    pub link: Option<Vec<u8>>,
    pub size: u64,
    pub mode: u32,
    pub uid: u64,
    pub gid: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
    /// `uname` and `gname`, when either is set.
    pub names: Option<(Vec<u8>, Vec<u8>)>,
    /// Device major and minor, for character and block devices.
    pub device: Option<(u64, u64)>,
    /// `LIBARCHIVE.creationtime`.
    pub btime: Option<(i64, u32)>,
    /// `SCHILY.fflags`, as recordable bits ([`meta::RECORDED_FLAGS`]).
    pub fflags: Option<u64>,
    /// Extended attributes, from `SCHILY.xattr.*` and `LIBARCHIVE.xattr.*`.
    pub xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    /// `SCHILY.acl.ace` text.
    pub acl: Option<Vec<u8>>,
    /// Bytes of header blocks: content starts this far into the member.
    pub header_len: u64,
}

fn parse_octal(field: &[u8]) -> Result<u64> {
    let digits = field
        .iter()
        .copied()
        .skip_while(|&b| b == b' ')
        .take_while(|&b| b != 0 && b != b' ');
    let mut value = 0u64;
    for d in digits {
        if !(b'0'..=b'7').contains(&d) {
            return format_err("a numeric tar field is not octal");
        }
        value = value
            .checked_mul(8)
            .and_then(|v| v.checked_add(u64::from(d - b'0')))
            .ok_or_else(|| Error::Format("a numeric tar field overflows".into()))?;
    }
    Ok(value)
}

fn cstr(field: &[u8]) -> &[u8] {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    &field[..end]
}

fn checksum_ok(block: &[u8]) -> Result<bool> {
    let stored = parse_octal(&block[CHKSUM])?;
    let mut unsigned = 0u64;
    let mut signed = 0i64;
    for (i, &b) in block.iter().enumerate() {
        let b = if CHKSUM.contains(&i) { b' ' } else { b };
        unsigned += u64::from(b);
        signed += i64::from(b as i8);
    }
    // POSIX sums bytes as unsigned; some historical writers summed them signed.
    Ok(stored == unsigned || stored as i64 == signed)
}

fn parse_pax_decimal(value: &[u8]) -> Result<u64> {
    core::str::from_utf8(value)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Error::Format("a pax numeric value is not a decimal integer".into()))
}

fn parse_pax_time(value: &[u8]) -> Result<(i64, u32)> {
    let s = core::str::from_utf8(value).map_err(|_| Error::Format("pax time not UTF-8".into()))?;
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    let negative = whole.starts_with('-');
    let whole: i64 = whole
        .parse()
        .map_err(|_| Error::Format(format!("pax time {s:?}")))?;
    let mut nanos = 0u32;
    for (i, d) in frac.bytes().take(9).enumerate() {
        if !d.is_ascii_digit() {
            return format_err(format!("pax time {s:?}"));
        }
        nanos += u32::from(d - b'0') * 10u32.pow(8 - i as u32);
    }
    if negative && nanos > 0 {
        // "-0.5" is half a second before the epoch: -1 s plus 0.5 s.
        Ok((whole - 1, 1_000_000_000 - nanos))
    } else {
        Ok((whole, nanos))
    }
}

/// Parse the member whose first header block starts at `stream[0]`.
///
/// `stream` must extend at least through the member's headers; its content is
/// not read.
pub fn parse_member(stream: &[u8]) -> Result<Member> {
    parse_member_prefix(stream)?
        .ok_or_else(|| Error::Format("tar stream ends inside a header".into()))
}

/// [`parse_member`] for a stream that arrives in pieces: `Ok(None)` when
/// `stream` ends before the member's headers do, so the caller can append the
/// next piece and ask again. Any other problem is an error as before.
pub fn parse_member_prefix(stream: &[u8]) -> Result<Option<Member>> {
    let mut pos = 0usize;
    let mut pax: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    loop {
        let Some(block) = stream.get(pos..pos + BLOCK) else {
            return Ok(None);
        };
        if block.iter().all(|&b| b == 0) {
            return format_err("an end-of-archive block where a member was expected");
        }
        if !checksum_ok(block)? {
            return format_err("tar header checksum mismatch");
        }
        if &block[MAGIC] != b"ustar\0" || &block[VERSION] != b"00" {
            return format_err("not a POSIX ustar header (§5.1)");
        }
        let typeflag = block[TYPEFLAG];
        let size = parse_octal(&block[SIZE])?;
        match typeflag {
            b'x' => {
                let end = (pos + BLOCK).checked_add(usize::try_from(size).unwrap_or(usize::MAX));
                let Some(records) = end.and_then(|end| stream.get(pos + BLOCK..end)) else {
                    return Ok(None);
                };
                parse_pax_records(records, &mut pax)?;
                pos += BLOCK + pad(size) as usize;
                continue;
            }
            b'g' => {
                return Err(Error::Unsupported(
                    "global pax headers (§5.3 says writers should not use them)".into(),
                ));
            }
            b'L' | b'K' => {
                return Err(Error::Unsupported(
                    "GNU long-name members; ZSTA requires pax (§5.1)".into(),
                ));
            }
            _ => {}
        }
        // An old-style regular file is typeflag NUL; it means the same as '0'.
        let flag = if typeflag == 0 { b'0' } else { typeflag };
        let kind = Kind::from_typeflag(flag)
            .ok_or_else(|| Error::Unsupported(format!("tar typeflag {:?}", flag as char)))?;

        let prefix = cstr(&block[PREFIX]);
        let mut path = prefix.to_vec();
        if !prefix.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(cstr(&block[NAME]));
        let mut m = Member {
            kind,
            path,
            link: matches!(kind, Kind::Symlink | Kind::HardLink)
                .then(|| cstr(&block[LINKNAME]).to_vec()),
            size,
            mode: parse_octal(&block[MODE])? as u32 & 0o7777,
            uid: parse_octal(&block[UID])?,
            gid: parse_octal(&block[GID])?,
            mtime_sec: parse_octal(&block[MTIME])? as i64,
            mtime_nsec: 0,
            names: None,
            device: None,
            btime: None,
            fflags: None,
            xattrs: Vec::new(),
            acl: None,
            header_len: (pos + BLOCK) as u64,
        };
        let (mut uname, mut gname) = (cstr(&block[UNAME]).to_vec(), cstr(&block[GNAME]).to_vec());
        if matches!(kind, Kind::CharDevice | Kind::BlockDevice) {
            m.device = Some((
                parse_octal(&block[DEVMAJOR])?,
                parse_octal(&block[DEVMINOR])?,
            ));
        }
        // §9.2: both xattr forms are accepted, and SCHILY wins a tie.
        let mut libarchive_xattrs = Vec::new();
        for (key, value) in pax {
            match key.as_slice() {
                b"path" => m.path = value,
                b"linkpath" => m.link = Some(value),
                b"size" => m.size = parse_pax_decimal(&value)?,
                b"uid" => m.uid = parse_pax_decimal(&value)?,
                b"gid" => m.gid = parse_pax_decimal(&value)?,
                b"mtime" => (m.mtime_sec, m.mtime_nsec) = parse_pax_time(&value)?,
                b"uname" => uname = value,
                b"gname" => gname = value,
                b"LIBARCHIVE.creationtime" => m.btime = Some(parse_pax_time(&value)?),
                b"SCHILY.fflags" => m.fflags = Some(meta::fflags_from_text(&value)),
                b"SCHILY.acl.ace" => m.acl = Some(value),
                b"SCHILY.devmajor" | b"SCHILY.devminor" => {
                    if let Some((major, minor)) = &mut m.device {
                        let n = parse_pax_decimal(&value)?;
                        *(if key.ends_with(b"major") {
                            major
                        } else {
                            minor
                        }) = n;
                    }
                }
                k if k.starts_with(b"SCHILY.xattr.") => {
                    m.xattrs.push((k[b"SCHILY.xattr.".len()..].to_vec(), value));
                }
                k if k.starts_with(b"LIBARCHIVE.xattr.") => {
                    let name = meta::url_decode(&k[b"LIBARCHIVE.xattr.".len()..]);
                    let value = meta::base64_decode(&value);
                    let (Some(name), Some(value)) = (name, value) else {
                        return format_err("a LIBARCHIVE.xattr record is not url/base64 encoded");
                    };
                    libarchive_xattrs.push((name, value));
                }
                _ => {}
            }
        }
        for (name, value) in libarchive_xattrs {
            if !m.xattrs.iter().any(|(n, _)| *n == name) {
                m.xattrs.push((name, value));
            }
        }
        if let Some(acl) = &m.acl {
            meta::parse_acl(acl)?; // refused here, not halfway through applying it
        }
        if !uname.is_empty() || !gname.is_empty() {
            m.names = Some((uname, gname));
        }
        // Directory names conventionally carry a trailing `/` in tar, which
        // the index (§7.1) does not; the type says what it is.
        if kind == Kind::Directory && m.path.len() > 1 && m.path.ends_with(b"/") {
            m.path.pop();
        }
        if !kind.has_content() {
            m.size = 0;
        }
        validate_path(&m.path)?;
        return Ok(Some(m));
    }
}

fn parse_pax_records(mut records: &[u8], out: &mut Vec<(Vec<u8>, Vec<u8>)>) -> Result<()> {
    while !records.is_empty() {
        let space = records
            .iter()
            .position(|&b| b == b' ')
            .ok_or_else(|| Error::Format("pax record without a length".into()))?;
        let len = parse_pax_decimal(&records[..space])? as usize;
        if len <= space + 1 || len > records.len() || records[len - 1] != b'\n' {
            return format_err("pax record length is wrong");
        }
        let body = &records[space + 1..len - 1];
        let eq = body
            .iter()
            .position(|&b| b == b'=')
            .ok_or_else(|| Error::Format("pax record without `=`".into()))?;
        out.push((body[..eq].to_vec(), body[eq + 1..].to_vec()));
        records = &records[len..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(e: &Entry) -> (Vec<u8>, Member) {
        let mut out = Vec::new();
        let header_len = write_headers(&mut out, e, &Extras::default()).unwrap();
        assert_eq!(out.len() as u64, header_len);
        assert_eq!(out.len() % BLOCK, 0);
        let m = parse_member(&out).unwrap();
        (out, m)
    }

    #[test]
    fn a_plain_file_needs_no_pax_header() {
        let mut e = Entry::new(Kind::File, "dir/file.txt");
        e.size = 1234;
        e.mtime_sec = 1_700_000_000;
        e.mode = 0o640;
        let (bytes, m) = member(&e);
        assert_eq!(bytes.len(), BLOCK, "one ustar block, no pax");
        assert_eq!(
            (m.path.as_slice(), m.size, m.mode),
            (&b"dir/file.txt"[..], 1234, 0o640)
        );
        assert_eq!(m.mtime_sec, 1_700_000_000);
    }

    #[test]
    fn every_section_9_1_case_gets_a_pax_record_and_round_trips() {
        let long: Vec<u8> = b"d/".repeat(200).into_iter().chain(*b"f").collect();
        let cases: Vec<Entry> = vec![
            Entry {
                path: long.clone(),
                size: 1,
                ..Entry::new(Kind::File, "")
            },
            Entry {
                path: "données/résumé.txt".into(),
                size: 1,
                ..Entry::new(Kind::File, "")
            },
            Entry {
                size: 9 << 30,
                ..Entry::new(Kind::File, "big")
            },
            Entry {
                uid: 1 << 40,
                gid: 3 << 30,
                ..Entry::new(Kind::File, "ids")
            },
            Entry {
                mtime_sec: 5,
                mtime_nsec: 250_000_000,
                ..Entry::new(Kind::File, "sub")
            },
            Entry {
                mtime_sec: -1,
                mtime_nsec: 500_000_000,
                ..Entry::new(Kind::File, "old")
            },
            Entry {
                link: Some(long.clone()),
                ..Entry::new(Kind::Symlink, "ln")
            },
        ];
        for e in cases {
            let (bytes, m) = member(&e);
            assert!(bytes.len() > BLOCK, "{} needed pax", e.path_lossy());
            assert_eq!(bytes[TYPEFLAG], b'x');
            assert_eq!(m.path, e.path);
            assert_eq!(m.size, e.size);
            assert_eq!((m.uid, m.gid), (e.uid, e.gid));
            assert_eq!((m.mtime_sec, m.mtime_nsec), (e.mtime_sec, e.mtime_nsec));
            assert_eq!(m.link, e.link);
        }
    }

    #[test]
    fn a_path_that_fits_ustar_with_its_prefix_uses_it() {
        let path: Vec<u8> = [b"p".repeat(120), b"/".to_vec(), b"n".repeat(90)].concat();
        let e = Entry::new(Kind::Directory, path.clone());
        let (bytes, m) = member(&e);
        assert_eq!(bytes.len(), BLOCK, "prefix + name, no pax");
        assert_eq!(m.path, path);
    }

    #[test]
    fn pax_record_lengths_count_themselves() {
        // The awkward case: the body is 9 bytes, so "9 ..." would be 10 long.
        for value_len in 0..300 {
            let mut out = Vec::new();
            pax_record(&mut out, "k", &vec![b'v'; value_len]);
            let space = out.iter().position(|&b| b == b' ').unwrap();
            let declared: usize = core::str::from_utf8(&out[..space])
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(declared, out.len(), "value of {value_len}");
        }
    }

    #[test]
    fn macos_metadata_keywords_round_trip() {
        let mut e = Entry::new(Kind::CharDevice, "dev/thing");
        e.btime = Some((-2, 750_000_000));
        e.names = Some((
            "dänïel-with-a-name-longer-than-32-bytes".into(),
            b"staff".to_vec(),
        ));
        e.fflags = Some(meta::flag::UF_HIDDEN | meta::flag::SF_IMMUTABLE);
        e.device = Some((1 << 22, 7)); // past the octal field: SCHILY.devmajor
        let extras = Extras {
            xattrs: vec![
                (b"com.apple.ResourceFork".to_vec(), vec![0u8, 1, b'\n', 255]),
                (b"user.empty".to_vec(), vec![]),
            ],
            acl: Some(b"user:dan:rw::allow:501,group:staff:x:fd:deny:20".to_vec()),
        };
        let mut out = Vec::new();
        write_headers(&mut out, &e, &extras).unwrap();
        let m = parse_member(&out).unwrap();
        assert_eq!(m.btime, e.btime);
        assert_eq!(m.names, e.names);
        assert_eq!(m.fflags, e.fflags);
        assert_eq!(m.device, e.device);
        assert_eq!(m.xattrs, extras.xattrs);
        assert_eq!(m.acl, extras.acl);
    }

    #[test]
    fn libarchive_xattrs_are_read_and_schily_wins_a_tie() {
        let e = Entry::new(Kind::File, "f");
        let mut records = Vec::new();
        pax_record(&mut records, "LIBARCHIVE.xattr.user.a%3Db", b"cGxhaW4");
        pax_record(&mut records, "LIBARCHIVE.xattr.user.both", b"bG9zZXI");
        pax_record(&mut records, "SCHILY.xattr.user.both", b"winner");
        let mut out =
            header_block(&e, b'x', records.len() as u64, (&[], b"PaxHeader/f"), &[]).to_vec();
        out.extend_from_slice(&records);
        out.resize(BLOCK + pad(records.len() as u64) as usize, 0);
        out.extend_from_slice(&header_block(&e, b'0', 0, (&[], b"f"), &[]));
        let m = parse_member(&out).unwrap();
        assert_eq!(
            m.xattrs,
            [
                (b"user.both".to_vec(), b"winner".to_vec()),
                (b"user.a=b".to_vec(), b"plain".to_vec()),
            ]
        );
    }

    #[test]
    fn an_xattr_name_that_cannot_be_a_keyword_is_refused() {
        let extras = Extras {
            xattrs: vec![(b"user.a=b".to_vec(), b"v".to_vec())],
            acl: None,
        };
        let r = write_headers(&mut Vec::new(), &Entry::new(Kind::File, "f"), &extras);
        assert!(matches!(r, Err(Error::Refused(_))));
    }

    #[test]
    fn malformed_acl_text_is_refused_at_parse() {
        let e = Entry::new(Kind::File, "f");
        let mut records = Vec::new();
        pax_record(&mut records, "SCHILY.acl.ace", b"wizard:x:r::allow");
        let mut out =
            header_block(&e, b'x', records.len() as u64, (&[], b"PaxHeader/f"), &[]).to_vec();
        out.extend_from_slice(&records);
        out.resize(BLOCK + pad(records.len() as u64) as usize, 0);
        out.extend_from_slice(&header_block(&e, b'0', 0, (&[], b"f"), &[]));
        assert!(parse_member(&out).is_err());
    }

    #[test]
    fn a_corrupt_header_is_refused() {
        let (bytes, _) = member(&Entry::new(Kind::File, "a"));
        let mut bad = bytes.clone();
        bad[0] = b'b';
        assert!(parse_member(&bad).is_err(), "checksum");
        assert!(parse_member(&[0u8; BLOCK]).is_err(), "end marker");
        let mut gnu = bytes.clone();
        gnu[MAGIC].copy_from_slice(b"ustar ");
        assert!(parse_member(&gnu).is_err(), "old GNU magic");
    }

    #[test]
    fn unsafe_paths_are_refused_from_tar_headers_too() {
        // §13 applies to tar headers as well as the index: the writer refuses
        // them, so build the header by hand.
        let e = Entry::new(Kind::File, "x");
        let mut b = header_block(&e, b'0', 0, (&[], b"../../etc/passwd"), &[]);
        let sum: u64 = {
            b[CHKSUM].fill(b' ');
            b.iter().map(|&x| u64::from(x)).sum()
        };
        b[CHKSUM.start..CHKSUM.start + 6].copy_from_slice(format!("{sum:06o}").as_bytes());
        assert!(matches!(parse_member(&b), Err(Error::Refused(_))));
    }
}
