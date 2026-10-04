//! The Central Index (spec §7): one record per tar member, in stream order.
//!
//! Written from the spec without reference to the probe's implementation, on
//! purpose: the probe's decoder then stands as an independent second reading
//! of §7 — the role `ruzstd` plays for the codec — and a misreading shared by
//! both would have to be made twice, independently.
//!
//! ## Untrusted, field by field
//!
//! An index is the part of an archive a hostile writer controls most directly:
//! it says where things are, and a reader acts on it before decoding any data.
//! So every field is bounded here, at parse time — counts against limits,
//! offsets against each other, paths against §7.1's rules — and the reader
//! bounds the offsets again against the tar stream (§13, "Index trust").

use zalt_codec::params::DecodeLimits;

use crate::frames;
use crate::layout::le64;
use crate::varint::{self, VarIntError};
use crate::{Error, Result, format_err};

/// Start of the Central Index `User_Data`.
pub const INDEX_MAGIC: [u8; 4] = *b"ZSTI";
/// `Index_Version` this crate reads and writes.
pub const INDEX_VERSION: u8 = 1;
/// Bytes before the index body.
const FIXED_LEN: usize = 24;

/// What a tar member is (§7.1, `Typeflag`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    File,
    HardLink,
    Symlink,
    CharDevice,
    BlockDevice,
    Directory,
    Fifo,
}

impl Kind {
    /// The tar typeflag byte.
    pub fn typeflag(self) -> u8 {
        match self {
            Self::File => b'0',
            Self::HardLink => b'1',
            Self::Symlink => b'2',
            Self::CharDevice => b'3',
            Self::BlockDevice => b'4',
            Self::Directory => b'5',
            Self::Fifo => b'6',
        }
    }

    /// The kind a typeflag names, if it is one §7.1 lists.
    pub fn from_typeflag(flag: u8) -> Option<Self> {
        Some(match flag {
            b'0' => Self::File,
            b'1' => Self::HardLink,
            b'2' => Self::Symlink,
            b'3' => Self::CharDevice,
            b'4' => Self::BlockDevice,
            b'5' => Self::Directory,
            b'6' => Self::Fifo,
            _ => return None,
        })
    }

    /// Whether the member stores content in the tar stream.
    pub fn has_content(self) -> bool {
        self == Self::File
    }
}

/// Presence bits (§7.2).
mod bit {
    pub const HASH: u64 = 1 << 0;
    pub const LINK: u64 = 1 << 1;
    pub const BTIME: u64 = 1 << 2;
    pub const NAMES: u64 = 1 << 3;
    pub const FFLAGS: u64 = 1 << 4;
    pub const XATTRS: u64 = 1 << 5;
    pub const DEVICE: u64 = 1 << 6;
    pub const SPARSE: u64 = 1 << 7;
    pub const EXT: u64 = 1 << 8;
    pub const KNOWN: u64 = (1 << 9) - 1;
}

/// One archive member, as the index records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    /// The member's path, as bytes: the spec stores paths exactly as the
    /// filesystem returned them (§9.3) and only *recommends* UTF-8.
    pub path: Vec<u8>,
    /// `st_mode & 07777`.
    pub mode: u32,
    pub uid: u64,
    pub gid: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
    /// Stream offset of the member's first header block.
    pub header_offset: u64,
    /// Stream offset of its first content block.
    pub data_offset: u64,
    /// Content bytes stored in the tar stream; 0 for everything but files.
    pub size: u64,
    /// Symlink or hardlink target.
    pub link: Option<Vec<u8>>,
    pub btime: Option<(i64, u32)>,
    /// User and group names.
    pub names: Option<(Vec<u8>, Vec<u8>)>,
    /// BSD file flags.
    pub fflags: Option<u64>,
    /// Extended attribute names and value sizes; the values live in the tar
    /// stream (§9.2).
    pub xattrs: Vec<(Vec<u8>, u64)>,
    /// Device major and minor.
    pub device: Option<(u64, u64)>,
    /// Logical size of a sparse file; `size` is then the stored size.
    pub sparse: Option<u64>,
    /// Content digest under the index's `Hash_Algorithm`.
    pub hash: Option<Vec<u8>>,
    /// Raw `EXT` bytes: extension TLVs, validated but not interpreted.
    pub ext: Option<Vec<u8>>,
}

impl Entry {
    /// A member with only the required fields set.
    pub fn new(kind: Kind, path: impl Into<Vec<u8>>) -> Self {
        Self {
            kind,
            path: path.into(),
            mode: 0o644,
            uid: 0,
            gid: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            header_offset: 0,
            data_offset: 0,
            size: 0,
            link: None,
            btime: None,
            names: None,
            fflags: None,
            xattrs: Vec::new(),
            device: None,
            sparse: None,
            hash: None,
            ext: None,
        }
    }

    /// The path for display; lossy where it is not UTF-8.
    pub fn path_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.path)
    }

    fn presence(&self) -> u64 {
        let mut p = 0;
        let set = |cond: bool, b: u64, p: &mut u64| {
            if cond {
                *p |= b;
            }
        };
        set(self.hash.is_some(), bit::HASH, &mut p);
        set(self.link.is_some(), bit::LINK, &mut p);
        set(self.btime.is_some(), bit::BTIME, &mut p);
        set(self.names.is_some(), bit::NAMES, &mut p);
        set(self.fflags.is_some(), bit::FFLAGS, &mut p);
        set(!self.xattrs.is_empty(), bit::XATTRS, &mut p);
        set(self.device.is_some(), bit::DEVICE, &mut p);
        set(self.sparse.is_some(), bit::SPARSE, &mut p);
        set(self.ext.is_some(), bit::EXT, &mut p);
        p
    }
}

/// The rules §7.1 puts on a path, which §13 also requires of tar headers.
///
/// Relative, `/`-separated, with no `..` component, no empty component (so no
/// leading, trailing or doubled `/`), and no NUL — tar names are C strings, so
/// a NUL would make the index and the header name different files.
pub fn validate_path(path: &[u8]) -> Result<()> {
    let refuse = |why: &str| {
        Err(Error::Refused(format!(
            "path {:?} {why}",
            String::from_utf8_lossy(path)
        )))
    };
    if path.is_empty() {
        return refuse("is empty");
    }
    if path.contains(&0) {
        return refuse("contains a NUL byte");
    }
    if path[0] == b'/' {
        return refuse("is absolute");
    }
    for component in path.split(|&b| b == b'/') {
        if component.is_empty() {
            return refuse("has an empty component");
        }
        if component == b".." {
            return refuse("has a `..` component");
        }
    }
    Ok(())
}

/// Digest length for a `Hash_Algorithm` (§7.3).
pub fn digest_len(algorithm: u8) -> Option<usize> {
    match algorithm {
        0 => Some(0),
        1 => Some(16),
        2 | 3 => Some(32),
        _ => None,
    }
}

/// Bounds on what an index may make a reader do.
#[derive(Debug, Clone, Copy)]
pub struct IndexLimits {
    pub max_entries: u64,
    /// Decoded size of the index body.
    pub max_body: u64,
    pub max_path: usize,
    pub max_xattrs_per_entry: u64,
}

impl Default for IndexLimits {
    fn default() -> Self {
        Self {
            max_entries: 10_000_000,
            max_body: 1 << 30,
            max_path: 4096,
            max_xattrs_per_entry: 4096,
        }
    }
}

/// A decoded Central Index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    pub hash_algorithm: u8,
    /// Length of the tar stream, Terminator included.
    pub tar_stream_size: u64,
    pub entries: Vec<Entry>,
}

impl Index {
    /// The Central Index's `User_Data`, its body compressed at `level`.
    pub fn encode(&self, level: i32) -> Result<Vec<u8>> {
        let Some(digest) = digest_len(self.hash_algorithm) else {
            return Err(Error::Unsupported(format!(
                "Hash_Algorithm {}",
                self.hash_algorithm
            )));
        };
        let mut body = Vec::new();
        let mut previous: &[u8] = &[];
        let mut last_offset = None;
        for e in &self.entries {
            validate_path(&e.path)?;
            if last_offset.is_some_and(|o| e.header_offset <= o) {
                return Err(Error::Refused("entries out of stream order".into()));
            }
            last_offset = Some(e.header_offset);
            if e.hash.as_ref().is_some_and(|h| h.len() != digest)
                || (digest == 0 && e.hash.is_some())
            {
                return Err(Error::Refused(
                    "entry hash does not match Hash_Algorithm".into(),
                ));
            }
            encode_entry(&mut body, e, previous)?;
            previous = &e.path;
        }

        let frame = frames::encode(&body, level)?;
        let mut out = Vec::with_capacity(FIXED_LEN + frame.len() + 8);
        out.extend_from_slice(&INDEX_MAGIC);
        out.push(INDEX_VERSION);
        out.push(self.hash_algorithm);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(self.entries.len() as u64).to_le_bytes());
        out.extend_from_slice(&self.tar_stream_size.to_le_bytes());
        out.extend_from_slice(&frame);
        let checksum = zalt_codec::xxh64::xxh64(&out, 0);
        out.extend_from_slice(&checksum.to_le_bytes());
        Ok(out)
    }

    /// Parse and validate a Central Index's `User_Data`.
    pub fn parse(data: &[u8], limits: &IndexLimits) -> Result<Self> {
        if data.len() < FIXED_LEN + 8 {
            return format_err("Central Index shorter than its fixed fields");
        }
        if data[..4] != INDEX_MAGIC {
            return format_err("Central Index does not begin with \"ZSTI\"");
        }
        // The checksum first: it is cheap, and it means nothing below ever
        // decompresses or walks bytes the writer did not produce.
        let (covered, stored) = data.split_at(data.len() - 8);
        if zalt_codec::xxh64::xxh64(covered, 0) != le64(stored) {
            return format_err("Index_Checksum does not match");
        }
        if data[4] != INDEX_VERSION {
            return Err(Error::Unsupported(format!("Index_Version {}", data[4])));
        }
        let hash_algorithm = data[5];
        let Some(digest) = digest_len(hash_algorithm) else {
            return Err(Error::Unsupported(format!(
                "Hash_Algorithm {hash_algorithm}"
            )));
        };
        if u16::from_le_bytes([data[6], data[7]]) != 0 {
            return format_err("reserved Index_Flags set");
        }
        let count = le64(&data[8..]);
        let tar_stream_size = le64(&data[16..]);
        if count > limits.max_entries {
            return Err(Error::Refused(format!(
                "{count} entries, over the limit of {}",
                limits.max_entries
            )));
        }

        let frame = &data[FIXED_LEN..data.len() - 8];
        let header = frames::header_of(frame, &DecodeLimits::default())?;
        if !header.has_checksum {
            return format_err("the index body has no content checksum (§7)");
        }
        let Some(body_len) = header.frame_content_size else {
            return format_err("the index body has no Frame_Content_Size (§7)");
        };
        if body_len > limits.max_body {
            return Err(Error::Refused(format!(
                "index body of {body_len} bytes, over the limit of {}",
                limits.max_body
            )));
        }
        let body = frames::decode_exactly(frame, body_len)?;

        let mut cursor = Cursor { buf: &body, pos: 0 };
        // At least eleven bytes a record (typeflag and ten one-byte VarInts),
        // so a count the body cannot possibly hold is caught before a vector
        // is reserved for it.
        if count > body.len() as u64 / 11 + 1 {
            return format_err(format!(
                "{count} entries cannot fit a {}-byte body",
                body.len()
            ));
        }
        let mut entries = Vec::with_capacity(count as usize);
        let mut previous: Vec<u8> = Vec::new();
        for i in 0..count {
            let e = decode_entry(&mut cursor, &previous, digest, limits)
                .map_err(|e| prefix(e, &format!("entry {i}")))?;
            if let Some(last) = entries.last().map(|l: &Entry| l.header_offset)
                && e.header_offset <= last
            {
                return format_err(format!("entry {i}: Header_Offset does not increase"));
            }
            if e.data_offset > tar_stream_size || e.size > tar_stream_size - e.data_offset {
                return format_err(format!("entry {i} runs past the end of the tar stream"));
            }
            previous.clone_from(&e.path);
            entries.push(e);
        }
        if cursor.pos != body.len() {
            return format_err(format!(
                "{} bytes left over after {count} entries",
                body.len() - cursor.pos
            ));
        }
        Ok(Self {
            hash_algorithm,
            tar_stream_size,
            entries,
        })
    }
}

fn prefix(e: Error, what: &str) -> Error {
    match e {
        Error::Format(s) => Error::Format(format!("{what}: {s}")),
        Error::Refused(s) => Error::Refused(format!("{what}: {s}")),
        other => other,
    }
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    varint::put(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn encode_entry(out: &mut Vec<u8>, e: &Entry, previous: &[u8]) -> Result<()> {
    if !e.kind.has_content() && e.size != 0 {
        return Err(Error::Refused("only regular files carry content".into()));
    }
    let shared = previous
        .iter()
        .zip(&e.path)
        .take_while(|(a, b)| a == b)
        .count();
    out.push(e.kind.typeflag());
    varint::put(out, e.presence());
    varint::put(out, shared as u64);
    put_bytes(out, &e.path[shared..]);
    varint::put(out, u64::from(e.mode & 0o7777));
    varint::put(out, e.uid);
    varint::put(out, e.gid);
    varint::put_signed(out, e.mtime_sec);
    varint::put(out, u64::from(e.mtime_nsec));
    varint::put(out, e.header_offset);
    varint::put(out, e.data_offset - e.header_offset);
    varint::put(out, e.size);
    // Optional fields, in ascending bit order (§7.2).
    if let Some(h) = &e.hash {
        out.extend_from_slice(h);
    }
    if let Some(l) = &e.link {
        put_bytes(out, l);
    }
    if let Some((s, ns)) = e.btime {
        varint::put_signed(out, s);
        varint::put(out, u64::from(ns));
    }
    if let Some((u, g)) = &e.names {
        put_bytes(out, u);
        put_bytes(out, g);
    }
    if let Some(f) = e.fflags {
        varint::put(out, f);
    }
    if !e.xattrs.is_empty() {
        varint::put(out, e.xattrs.len() as u64);
        for (name, size) in &e.xattrs {
            put_bytes(out, name);
            varint::put(out, *size);
        }
    }
    if let Some((major, minor)) = e.device {
        varint::put(out, major);
        varint::put(out, minor);
    }
    if let Some(logical) = e.sparse {
        varint::put(out, logical);
    }
    if let Some(ext) = &e.ext {
        put_bytes(out, ext);
    }
    Ok(())
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn byte(&mut self) -> Result<u8> {
        let b = *self
            .buf
            .get(self.pos)
            .ok_or_else(|| Error::Format("record truncated".into()))?;
        self.pos += 1;
        Ok(b)
    }

    fn varint(&mut self) -> Result<u64> {
        let (v, used) = varint::get(&self.buf[self.pos..]).map_err(varint_err)?;
        self.pos += used;
        Ok(v)
    }

    fn signed(&mut self) -> Result<i64> {
        let (v, used) = varint::get_signed(&self.buf[self.pos..]).map_err(varint_err)?;
        self.pos += used;
        Ok(v)
    }

    fn take(&mut self, n: u64) -> Result<&'a [u8]> {
        let end = usize::try_from(n)
            .ok()
            .and_then(|n| self.pos.checked_add(n))
            .filter(|&end| end <= self.buf.len())
            .ok_or_else(|| Error::Format("field runs past the index body".into()))?;
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.varint()?;
        self.take(len)
    }

    fn nanos(&mut self) -> Result<u32> {
        let ns = self.varint()?;
        if ns > 999_999_999 {
            return format_err(format!("{ns} nanoseconds"));
        }
        Ok(ns as u32)
    }
}

fn varint_err(e: VarIntError) -> Error {
    Error::Format(e.to_string())
}

fn decode_entry(
    c: &mut Cursor<'_>,
    previous: &[u8],
    digest: usize,
    limits: &IndexLimits,
) -> Result<Entry> {
    let flag = c.byte()?;
    let kind = Kind::from_typeflag(flag)
        .ok_or_else(|| Error::Format(format!("unknown typeflag {:?}", flag as char)))?;
    let presence = c.varint()?;
    if presence & !bit::KNOWN != 0 {
        return format_err(format!("reserved presence bits {presence:#x}"));
    }
    let shared = c.varint()?;
    if shared > previous.len() as u64 {
        return format_err("Path_Shared longer than the previous path");
    }
    let suffix = c.bytes()?;
    let mut path = previous[..shared as usize].to_vec();
    path.extend_from_slice(suffix);
    if path.len() > limits.max_path {
        return Err(Error::Refused(format!("a path of {} bytes", path.len())));
    }
    validate_path(&path)?;

    let mode = c.varint()?;
    if mode > 0o7777 {
        return format_err(format!("mode {mode:o} has bits outside 07777"));
    }
    let uid = c.varint()?;
    let gid = c.varint()?;
    let mtime_sec = c.signed()?;
    let mtime_nsec = c.nanos()?;
    let header_offset = c.varint()?;
    let delta = c.varint()?;
    let size = c.varint()?;
    // Tar is built of 512-byte blocks, and a member's headers take at least
    // one; anything else cannot be a header offset in a valid tar stream.
    if header_offset % 512 != 0 || delta < 512 || delta % 512 != 0 {
        return format_err("offsets are not on 512-byte tar block boundaries");
    }
    let data_offset = header_offset
        .checked_add(delta)
        .ok_or_else(|| Error::Format("data offset overflows".into()))?;
    if !kind.has_content() && size != 0 {
        return format_err(format!("a {kind:?} cannot store content"));
    }

    let mut e = Entry {
        kind,
        path,
        mode: mode as u32,
        uid,
        gid,
        mtime_sec,
        mtime_nsec,
        header_offset,
        data_offset,
        size,
        ..Entry::new(kind, Vec::new())
    };

    if presence & bit::HASH != 0 {
        if digest == 0 {
            return format_err("HASH present under Hash_Algorithm 0");
        }
        e.hash = Some(c.take(digest as u64)?.to_vec());
    }
    if presence & bit::LINK != 0 {
        e.link = Some(c.bytes()?.to_vec());
    }
    if matches!(kind, Kind::Symlink | Kind::HardLink) != e.link.is_some() {
        return format_err("LINK must be present exactly on symlinks and hardlinks");
    }
    if presence & bit::BTIME != 0 {
        e.btime = Some((c.signed()?, c.nanos()?));
    }
    if presence & bit::NAMES != 0 {
        e.names = Some((c.bytes()?.to_vec(), c.bytes()?.to_vec()));
    }
    if presence & bit::FFLAGS != 0 {
        e.fflags = Some(c.varint()?);
    }
    if presence & bit::XATTRS != 0 {
        let count = c.varint()?;
        if count > limits.max_xattrs_per_entry {
            return Err(Error::Refused(format!("{count} extended attributes")));
        }
        for _ in 0..count {
            let name = c.bytes()?.to_vec();
            let value_size = c.varint()?;
            e.xattrs.push((name, value_size));
        }
    }
    if presence & bit::DEVICE != 0 {
        e.device = Some((c.varint()?, c.varint()?));
    }
    if presence & bit::SPARSE != 0 {
        let logical = c.varint()?;
        if logical < size {
            return format_err("sparse logical size below the stored size");
        }
        e.sparse = Some(logical);
    }
    if presence & bit::EXT != 0 {
        let ext = c.bytes()?;
        validate_ext(ext)?;
        e.ext = Some(ext.to_vec());
    }
    Ok(e)
}

/// `EXT` holds TLVs a reader skips if it does not know them (§7.4) — but it
/// must be able to *walk* them, so the structure is checked even though no tag
/// is defined yet.
fn validate_ext(ext: &[u8]) -> Result<()> {
    let mut c = Cursor { buf: ext, pos: 0 };
    while c.pos < ext.len() {
        let _tag = c.varint()?;
        let len = c.varint()?;
        c.take(len)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Index {
        let mut a = Entry::new(Kind::Directory, "proj");
        a.mode = 0o755;
        a.header_offset = 0;
        a.data_offset = 512;
        let mut b = Entry::new(Kind::File, "proj/a.txt");
        b.header_offset = 512;
        b.data_offset = 1024;
        b.size = 5;
        b.mtime_sec = -3;
        b.mtime_nsec = 999_999_999;
        b.names = Some((b"dan".to_vec(), b"staff".to_vec()));
        b.xattrs = vec![(b"com.apple.FinderInfo".to_vec(), 32)];
        let mut c = Entry::new(Kind::Symlink, "proj/latest");
        c.header_offset = 1536;
        c.data_offset = 2048;
        c.link = Some(b"a.txt".to_vec());
        let mut d = Entry::new(Kind::File, "proj/données/résumé.txt".as_bytes());
        d.header_offset = 2048;
        d.data_offset = 3072; // a pax header first
        d.size = 1;
        d.ext = Some(vec![0x05, 0x02, 0xAA, 0xBB]);
        Index {
            hash_algorithm: 0,
            tar_stream_size: 5120,
            entries: vec![a, b, c, d],
        }
    }

    #[test]
    fn an_index_round_trips() {
        let idx = sample();
        let data = idx.encode(3).unwrap();
        assert_eq!(&data[..4], b"ZSTI");
        assert_eq!(Index::parse(&data, &IndexLimits::default()).unwrap(), idx);
    }

    #[test]
    fn paths_share_prefixes_with_the_previous_record() {
        let idx = sample();
        let mut body = Vec::new();
        let mut previous: &[u8] = &[];
        for e in &idx.entries {
            encode_entry(&mut body, e, previous).unwrap();
            previous = &e.path;
        }
        // "proj/a.txt" after "proj" stores only "/a.txt".
        let needle = b"\x06/a.txt";
        assert!(body.windows(needle.len()).any(|w| w == needle));
    }

    #[test]
    fn a_single_flipped_byte_anywhere_is_caught_by_the_checksum() {
        let data = sample().encode(3).unwrap();
        for i in 0..data.len() {
            let mut bad = data.clone();
            bad[i] ^= 0x01;
            assert!(
                Index::parse(&bad, &IndexLimits::default()).is_err(),
                "byte {i}"
            );
        }
    }

    /// Re-sign a doctored index, so the field checks are tested rather than
    /// the checksum standing in front of them.
    fn resign(mut data: Vec<u8>) -> Vec<u8> {
        let n = data.len() - 8;
        let sum = zalt_codec::xxh64::xxh64(&data[..n], 0);
        data[n..].copy_from_slice(&sum.to_le_bytes());
        data
    }

    #[test]
    fn fixed_fields_are_checked_behind_the_checksum() {
        let data = sample().encode(3).unwrap();
        let limits = IndexLimits::default();
        let mut bad = data.clone();
        bad[4] = 2;
        assert!(matches!(
            Index::parse(&resign(bad), &limits),
            Err(Error::Unsupported(_))
        ));
        let mut bad = data.clone();
        bad[5] = 9;
        assert!(matches!(
            Index::parse(&resign(bad), &limits),
            Err(Error::Unsupported(_))
        ));
        let mut bad = data.clone();
        bad[6] = 1;
        assert!(Index::parse(&resign(bad), &limits).is_err(), "Index_Flags");
        let mut bad = data.clone();
        bad[8] = 5;
        assert!(Index::parse(&resign(bad), &limits).is_err(), "Entry_Count");
        let tight = IndexLimits {
            max_entries: 3,
            ..limits
        };
        assert!(matches!(
            Index::parse(&data, &tight),
            Err(Error::Refused(_))
        ));
    }

    #[test]
    fn records_are_validated_as_they_are_read() {
        // Each doctored index is encoded through a path that skips the
        // writer's own checks, so the reader's are what is tested.
        type Doctor = Box<dyn Fn(&mut Index)>;
        let cases: Vec<(&str, Doctor)> = vec![
            (
                "dotdot",
                Box::new(|i| i.entries[1].path = b"proj/../etc".to_vec()),
            ),
            (
                "absolute",
                Box::new(|i| i.entries[1].path = b"/etc/passwd".to_vec()),
            ),
            (
                "empty component",
                Box::new(|i| i.entries[1].path = b"proj//a".to_vec()),
            ),
            (
                "trailing slash",
                Box::new(|i| i.entries[0].path = b"proj/".to_vec()),
            ),
            (
                "nul",
                Box::new(|i| i.entries[1].path = b"proj/a\0b".to_vec()),
            ),
            ("order", Box::new(|i| i.entries[2].header_offset = 512)),
            ("unaligned", Box::new(|i| i.entries[1].header_offset = 513)),
            ("content on a dir", Box::new(|i| i.entries[0].size = 1)),
            (
                "link on a file",
                Box::new(|i| i.entries[1].link = Some(b"x".to_vec())),
            ),
            (
                "symlink without link",
                Box::new(|i| i.entries[2].link = None),
            ),
            ("past the stream", Box::new(|i| i.entries[3].size = 1 << 20)),
            ("mode", Box::new(|i| i.entries[1].mode = 0o170_000)),
        ];
        for (name, doctor) in cases {
            let mut idx = sample();
            doctor(&mut idx);
            let data = encode_unchecked(&idx);
            assert!(
                Index::parse(&data, &IndexLimits::default()).is_err(),
                "{name} was accepted"
            );
        }
    }

    /// Encode without the writer-side validation, for the test above.
    fn encode_unchecked(idx: &Index) -> Vec<u8> {
        let mut body = Vec::new();
        let mut previous: Vec<u8> = Vec::new();
        for e in &idx.entries {
            let shared = previous
                .iter()
                .zip(&e.path)
                .take_while(|(a, b)| a == b)
                .count();
            body.push(e.kind.typeflag());
            varint::put(&mut body, e.presence());
            varint::put(&mut body, shared as u64);
            put_bytes(&mut body, &e.path[shared..]);
            varint::put(&mut body, u64::from(e.mode));
            varint::put(&mut body, e.uid);
            varint::put(&mut body, e.gid);
            varint::put_signed(&mut body, e.mtime_sec);
            varint::put(&mut body, u64::from(e.mtime_nsec));
            varint::put(&mut body, e.header_offset);
            varint::put(&mut body, e.data_offset.wrapping_sub(e.header_offset));
            varint::put(&mut body, e.size);
            if let Some(l) = &e.link {
                put_bytes(&mut body, l);
            }
            if let Some((u, g)) = &e.names {
                put_bytes(&mut body, u);
                put_bytes(&mut body, g);
            }
            if !e.xattrs.is_empty() {
                varint::put(&mut body, e.xattrs.len() as u64);
                for (n, s) in &e.xattrs {
                    put_bytes(&mut body, n);
                    varint::put(&mut body, *s);
                }
            }
            if let Some(ext) = &e.ext {
                put_bytes(&mut body, ext);
            }
            previous.clone_from(&e.path);
        }
        let frame = frames::encode(&body, 3).unwrap();
        let mut out = Vec::new();
        out.extend_from_slice(&INDEX_MAGIC);
        out.push(INDEX_VERSION);
        out.push(idx.hash_algorithm);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(idx.entries.len() as u64).to_le_bytes());
        out.extend_from_slice(&idx.tar_stream_size.to_le_bytes());
        out.extend_from_slice(&frame);
        let sum = zalt_codec::xxh64::xxh64(&out, 0);
        out.extend_from_slice(&sum.to_le_bytes());
        out
    }

    #[test]
    fn the_unchecked_encoder_agrees_with_the_real_one_on_valid_input() {
        // Otherwise the test above would be testing a different format.
        let idx = sample();
        assert_eq!(encode_unchecked(&idx), idx.encode(3).unwrap());
    }

    #[test]
    fn path_rules() {
        for good in [&b"a"[..], b"a/b", b"./a", b"a/.b", "données".as_bytes()] {
            assert!(
                validate_path(good).is_ok(),
                "{:?}",
                String::from_utf8_lossy(good)
            );
        }
        for bad in [&b""[..], b"/a", b"a/../b", b"..", b"a//b", b"a/", b"a\0b"] {
            assert!(
                validate_path(bad).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }
}
