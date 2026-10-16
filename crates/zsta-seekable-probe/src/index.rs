//! Central Index (§7) and the primitive encodings it uses (§2).

use crate::{
    encoder::FrameEncoder,
    error_name,
    frame::{self, FrameKind},
    is_error, le64,
    tar::TarEntry,
    xxh64::xxh64,
};

pub const INDEX_MAGIC: &[u8; 4] = b"ZSTI";
pub const INDEX_VERSION: u8 = 1;
pub const PRESENCE_HASH: u64 = 1 << 0;
pub const PRESENCE_LINK: u64 = 1 << 1;
const PRESENCE_KNOWN: u64 = (1 << 9) - 1;
const FIXED_HEADER: usize = 24;

// ---------------------------------------------------------------- §2 primitives

pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// Strict LEB128 decode: rejects >10 bytes, u64 overflow and non-minimal forms.
pub fn get_varint(buf: &[u8], pos: &mut usize) -> Result<u64, String> {
    let mut v = 0u64;
    for i in 0..10 {
        let b = *buf.get(*pos).ok_or("varint: truncated")?;
        *pos += 1;
        let bits = u64::from(b & 0x7f);
        if i == 9 && bits > 1 {
            return Err("varint: overflows u64".into());
        }
        v |= bits << (7 * i);
        if b & 0x80 == 0 {
            if i > 0 && b == 0 {
                return Err("varint: non-minimal encoding".into());
            }
            return Ok(v);
        }
    }
    Err("varint: longer than 10 bytes".into())
}

pub fn zigzag(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)) as u64
}

pub fn unzigzag(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

fn get_bytes<'a>(buf: &'a [u8], pos: &mut usize) -> Result<&'a [u8], String> {
    let n = get_varint(buf, pos)? as usize;
    let end = pos
        .checked_add(n)
        .filter(|&e| e <= buf.len())
        .ok_or("bytes: truncated")?;
    let s = &buf[*pos..end];
    *pos = end;
    Ok(s)
}

// ---------------------------------------------------------------- §7 index

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    pub typeflag: u8,
    pub path: String,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
    pub header_offset: u64,
    pub data_delta: u64,
    pub size: u64,
    /// The raw digest (§7.2 bit 0), its length fixed by `Hash_Algorithm`.
    pub hash: Option<Vec<u8>>,
    pub link: Option<String>,
}

impl IndexEntry {
    pub fn data_offset(&self) -> u64 {
        self.header_offset + self.data_delta
    }
}

#[derive(Clone, Debug)]
pub struct CentralIndex {
    pub hash_algorithm: u8,
    pub tar_stream_size: u64,
    pub entries: Vec<IndexEntry>,
}

/// Build the Central Index frame's User_Data (everything after the 8-byte
/// skippable header).
pub fn encode(
    entries: &[TarEntry],
    tar_stream_size: u64,
    enc: &mut dyn FrameEncoder,
) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    let mut prev: &[u8] = &[];
    for e in entries {
        let path = e.path.as_bytes();
        let shared = prev.iter().zip(path).take_while(|(a, b)| a == b).count();
        body.push(e.typeflag);
        put_varint(&mut body, if e.link.is_some() { PRESENCE_LINK } else { 0 });
        put_varint(&mut body, shared as u64);
        put_bytes(&mut body, &path[shared..]);
        put_varint(&mut body, u64::from(e.mode & 0o7777));
        put_varint(&mut body, u64::from(e.uid));
        put_varint(&mut body, u64::from(e.gid));
        put_varint(&mut body, zigzag(e.mtime));
        put_varint(&mut body, 0);
        put_varint(&mut body, e.header_offset);
        put_varint(&mut body, e.data_offset - e.header_offset);
        put_varint(&mut body, e.size);
        if let Some(l) = &e.link {
            put_bytes(&mut body, l.as_bytes());
        }
        prev = path;
    }

    let mut ud = Vec::with_capacity(FIXED_HEADER + body.len() / 2 + 8);
    ud.extend_from_slice(INDEX_MAGIC);
    ud.push(INDEX_VERSION);
    ud.push(0); // Hash_Algorithm: none
    ud.extend_from_slice(&0u16.to_le_bytes());
    ud.extend_from_slice(&(entries.len() as u64).to_le_bytes());
    ud.extend_from_slice(&tar_stream_size.to_le_bytes());
    ud.extend_from_slice(&enc.encode_frame(&body)?);
    let checksum = xxh64(&ud, 0);
    ud.extend_from_slice(&checksum.to_le_bytes());
    Ok(ud)
}

/// Parse and validate the Central Index User_Data. `max_body` bounds the
/// decompressed body size (§13 resource limits).
pub fn decode(ud: &[u8], max_body: usize) -> Result<CentralIndex, String> {
    if ud.len() < FIXED_HEADER + 8 {
        return Err("index: too short".into());
    }
    if &ud[0..4] != INDEX_MAGIC {
        return Err("index: bad magic".into());
    }
    if ud[4] != INDEX_VERSION {
        return Err(format!("index: unsupported version {}", ud[4]));
    }
    let hash_algorithm = ud[5];
    if ud[6] != 0 || ud[7] != 0 {
        return Err("index: reserved Index_Flags set".into());
    }
    let count = le64(&ud[8..]);
    let tar_stream_size = le64(&ud[16..]);
    let (covered, stored) = ud.split_at(ud.len() - 8);
    if xxh64(covered, 0) != le64(stored) {
        return Err("index: Index_Checksum mismatch".into());
    }

    let body_frame = &covered[FIXED_HEADER..];
    let (kind, csize) = frame::inspect(body_frame)?;
    let content_size = match kind {
        FrameKind::Zstd {
            content_size: Some(n),
            has_checksum: true,
            dict_id: 0,
            ..
        } => n,
        other => return Err(format!("index: body frame violates §7: {other:?}")),
    };
    if csize as usize != body_frame.len() {
        return Err("index: body is not exactly one frame".into());
    }
    if content_size as usize > max_body {
        return Err(format!("index: body of {content_size} bytes exceeds limit"));
    }
    let mut body = vec![0u8; content_size as usize];
    // SAFETY: destination has exactly content_size bytes; source is one whole frame.
    let n = unsafe {
        zstd_sys::ZSTD_decompress(
            body.as_mut_ptr().cast(),
            body.len(),
            body_frame.as_ptr().cast(),
            body_frame.len(),
        )
    };
    if is_error(n) || n != body.len() {
        return Err(format!("index: body decode failed: {}", error_name(n)));
    }

    // Each record is at least 12 bytes, which bounds a hostile Entry_Count.
    if count > (body.len() / 12) as u64 {
        return Err("index: Entry_Count exceeds body size".into());
    }
    let mut entries = Vec::with_capacity(count as usize);
    let mut pos = 0usize;
    let mut prev: Vec<u8> = Vec::new();
    let mut last_header: Option<u64> = None;
    for _ in 0..count {
        let typeflag = *body.get(pos).ok_or("index: truncated record")?;
        pos += 1;
        let presence = get_varint(&body, &mut pos)?;
        if presence & !PRESENCE_KNOWN != 0 {
            return Err("index: unknown presence bits".into());
        }
        let shared = get_varint(&body, &mut pos)? as usize;
        if shared > prev.len() {
            return Err("index: Path_Shared exceeds previous path".into());
        }
        let mut path = prev[..shared].to_vec();
        path.extend_from_slice(get_bytes(&body, &mut pos)?);
        let path = String::from_utf8(path).map_err(|_| "index: path is not UTF-8")?;
        validate_path(&path)?;
        let mode = get_varint(&body, &mut pos)? as u32;
        let uid = get_varint(&body, &mut pos)? as u32;
        let gid = get_varint(&body, &mut pos)? as u32;
        let mtime_sec = unzigzag(get_varint(&body, &mut pos)?);
        let mtime_nsec = get_varint(&body, &mut pos)? as u32;
        let header_offset = get_varint(&body, &mut pos)?;
        let data_delta = get_varint(&body, &mut pos)?;
        let size = get_varint(&body, &mut pos)?;
        // Optional fields in ascending bit order (§7.2): HASH, then LINK.
        let hash = if presence & PRESENCE_HASH != 0 {
            // §7.3's table: algorithm 0 forbids the bit, the rest fix a length.
            let len = match hash_algorithm {
                1 => 16,
                2 | 3 => 32,
                0 => return Err("index: HASH present under Hash_Algorithm 0".into()),
                n => return Err(format!("index: unknown Hash_Algorithm {n}")),
            };
            let end = pos.checked_add(len).ok_or("index: offset overflow")?;
            let digest = body.get(pos..end).ok_or("index: truncated HASH")?;
            pos = end;
            Some(digest.to_vec())
        } else {
            None
        };
        let link = if presence & PRESENCE_LINK != 0 {
            let l = get_bytes(&body, &mut pos)?;
            Some(String::from_utf8(l.to_vec()).map_err(|_| "index: link is not UTF-8")?)
        } else {
            None
        };
        if presence & !(PRESENCE_HASH | PRESENCE_LINK) != 0 {
            return Err(
                "index: probe does not parse optional fields other than HASH and LINK".into(),
            );
        }
        if last_header.is_some_and(|h| header_offset <= h) {
            return Err("index: Header_Offset not strictly increasing".into());
        }
        let end = header_offset
            .checked_add(data_delta)
            .and_then(|d| d.checked_add(size))
            .ok_or("index: offset overflow")?;
        if end > tar_stream_size {
            return Err("index: entry extends past Tar_Stream_Size".into());
        }
        last_header = Some(header_offset);
        prev = path.as_bytes().to_vec();
        entries.push(IndexEntry {
            typeflag,
            path,
            mode,
            uid,
            gid,
            mtime_sec,
            mtime_nsec,
            header_offset,
            data_delta,
            size,
            hash,
            link,
        });
    }
    if pos != body.len() {
        return Err("index: trailing bytes after last record".into());
    }
    Ok(CentralIndex {
        hash_algorithm,
        tar_stream_size,
        entries,
    })
}

fn validate_path(p: &str) -> Result<(), String> {
    if p.is_empty() || p.starts_with('/') || p.split('/').any(|c| c.is_empty() || c == "..") {
        return Err(format!("index: unsafe path {p:?}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip_and_strictness() {
        for v in [0, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            let mut b = Vec::new();
            put_varint(&mut b, v);
            let mut p = 0;
            assert_eq!(get_varint(&b, &mut p).unwrap(), v);
            assert_eq!(p, b.len());
        }
        assert!(get_varint(&[0x80, 0x00], &mut 0).is_err(), "non-minimal");
        assert!(get_varint(&[0xff; 10], &mut 0).is_err(), "overflow");
        assert!(get_varint(&[0x80; 11], &mut 0).is_err(), "too long");
        assert!(get_varint(&[0x80], &mut 0).is_err(), "truncated");
    }

    #[test]
    fn zigzag_roundtrip() {
        for n in [0i64, -1, 1, i64::MIN, i64::MAX, -1_790_000_000] {
            assert_eq!(unzigzag(zigzag(n)), n);
        }
    }
}
