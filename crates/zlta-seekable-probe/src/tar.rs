//! Minimal POSIX pax/ustar writer that records where each entry lands in the
//! tar stream (the `Header_Offset` / data offset of §5.3).
//!
//! Only what the probe needs: regular files, directories and symlinks, with a pax
//! `x` header for long or non-ASCII names. The end-of-archive marker is *not*
//! written; in ZLTA it lives in its own Terminator frame (§5.4).

pub const BLOCK: usize = 512;
pub const UID: u32 = 501;
pub const GID: u32 = 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TarEntry {
    /// Effective path, without a trailing `/` for directories (the index form, §7.1).
    pub path: String,
    pub typeflag: u8,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: i64,
    pub link: Option<String>,
    /// Stream offset of the entry's first header block (the pax header, if any).
    pub header_offset: u64,
    /// Stream offset of the entry's first content block.
    pub data_offset: u64,
    /// Stored content bytes.
    pub size: u64,
}

#[derive(Default)]
pub struct TarWriter {
    buf: Vec<u8>,
    entries: Vec<TarEntry>,
}

impl TarWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_file(&mut self, path: &str, data: &[u8], mode: u32, mtime: i64) {
        self.add(path, b'0', mode, mtime, None, data);
    }

    pub fn add_dir(&mut self, path: &str, mode: u32, mtime: i64) {
        self.add(path, b'5', mode, mtime, None, &[]);
    }

    pub fn add_symlink(&mut self, path: &str, target: &str, mtime: i64) {
        self.add(path, b'2', 0o755, mtime, Some(target), &[]);
    }

    /// Returns the tar body (a multiple of 512 bytes, no EOF marker) and the entries.
    pub fn finish(self) -> (Vec<u8>, Vec<TarEntry>) {
        (self.buf, self.entries)
    }

    fn add(
        &mut self,
        path: &str,
        typeflag: u8,
        mode: u32,
        mtime: i64,
        link: Option<&str>,
        data: &[u8],
    ) {
        assert!(!path.is_empty() && !path.starts_with('/') && !path.ends_with('/'));
        let header_offset = self.buf.len() as u64;
        let tar_name = if typeflag == b'5' {
            format!("{path}/")
        } else {
            path.to_string()
        };

        let pax_path = tar_name.len() > 100 || !tar_name.is_ascii();
        let pax_link = link.is_some_and(|l| l.len() > 100 || !l.is_ascii());
        if pax_path || pax_link {
            let mut records = Vec::new();
            if pax_path {
                records.extend(pax_record("path", tar_name.as_bytes()));
            }
            if let (true, Some(l)) = (pax_link, link) {
                records.extend(pax_record("linkpath", l.as_bytes()));
            }
            let pax_name = format!("PaxHeaders/{}", ascii_prefix(&tar_name, 80));
            self.push_header(&pax_name, b'x', 0o644, mtime, None, records.len() as u64);
            self.push_data(&records);
        }

        let short_link = link.map(|l| ascii_prefix(l, 100));
        self.push_header(
            &ascii_prefix(&tar_name, 100),
            typeflag,
            mode,
            mtime,
            short_link.as_deref(),
            data.len() as u64,
        );
        let data_offset = self.buf.len() as u64;
        self.push_data(data);

        self.entries.push(TarEntry {
            path: path.to_string(),
            typeflag,
            mode,
            uid: UID,
            gid: GID,
            mtime,
            link: link.map(str::to_string),
            header_offset,
            data_offset,
            size: data.len() as u64,
        });
    }

    fn push_header(
        &mut self,
        name: &str,
        typeflag: u8,
        mode: u32,
        mtime: i64,
        link: Option<&str>,
        size: u64,
    ) {
        let mut h = [0u8; BLOCK];
        put_str(&mut h[0..100], name);
        put_octal(&mut h[100..108], u64::from(mode));
        put_octal(&mut h[108..116], u64::from(UID));
        put_octal(&mut h[116..124], u64::from(GID));
        put_octal(&mut h[124..136], size);
        put_octal(&mut h[136..148], mtime.max(0) as u64);
        h[148..156].fill(b' ');
        h[156] = typeflag;
        if let Some(l) = link {
            put_str(&mut h[157..257], l);
        }
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        put_str(&mut h[265..297], "user");
        put_str(&mut h[297..329], "staff");
        put_octal(&mut h[329..337], 0);
        put_octal(&mut h[337..345], 0);
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        self.buf.extend_from_slice(&h);
    }

    fn push_data(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        let pad = (BLOCK - data.len() % BLOCK) % BLOCK;
        self.buf.resize(self.buf.len() + pad, 0);
    }
}

fn put_str(field: &mut [u8], s: &str) {
    let b = s.as_bytes();
    assert!(b.len() <= field.len(), "tar field overflow: {s}");
    field[..b.len()].copy_from_slice(b);
}

fn put_octal(field: &mut [u8], v: u64) {
    let width = field.len() - 1;
    let s = format!("{v:0width$o}");
    assert!(
        s.len() <= width,
        "value {v} does not fit a {width}-digit octal field"
    );
    field[..width].copy_from_slice(s.as_bytes());
    field[width] = 0;
}

/// ASCII-only prefix of `s`, at most `max` bytes (non-ASCII becomes `_`).
fn ascii_prefix(s: &str, max: usize) -> String {
    s.chars()
        .map(|c| if c.is_ascii() { c } else { '_' })
        .take(max)
        .collect()
}

/// One pax record: `"<len> <key>=<value>\n"`, where `<len>` counts itself.
fn pax_record(key: &str, value: &[u8]) -> Vec<u8> {
    let rest = 1 + key.len() + 1 + value.len() + 1;
    let mut total = rest + 1;
    loop {
        let t = rest + total.to_string().len();
        if t == total {
            break;
        }
        total = t;
    }
    let mut r = format!("{total} {key}=").into_bytes();
    r.extend_from_slice(value);
    r.push(b'\n');
    debug_assert_eq!(r.len(), total);
    r
}
