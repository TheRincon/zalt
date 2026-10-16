//! Minimal ZSTA writer (§3–§8), with switches for the layouts that Q1 compares.

use std::ops::Range;

use crate::{encoder::FrameEncoder, index, tar::TarEntry, xxh64::xxh64};

pub const MAGIC_HEADER: u32 = 0x184D_2A50;
pub const MAGIC_DICT: u32 = 0x184D_2A51;
pub const MAGIC_INDEX: u32 = 0x184D_2A52;
pub const MAGIC_SEEK_TABLE: u32 = 0x184D_2A5E;
pub const MAGIC_VENDOR: u32 = 0x184D_2A5F;
pub const SEEKABLE_FOOTER_MAGIC: u32 = 0x8F92_EAB1;
pub const TERMINATOR_LEN: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Header,
    Dictionary,
    Vendor,
    Data,
    Terminator,
    Index,
}

impl Role {
    pub fn is_zstd(self) -> bool {
        matches!(self, Role::Data | Role::Terminator)
    }
}

#[derive(Clone, Debug)]
pub struct FrameRecord {
    pub role: Role,
    pub file_offset: u64,
    /// Full on-disk size, including the 8-byte header for skippable frames.
    pub csize: u64,
    pub stream_offset: u64,
    pub dsize: u64,
    /// Low 32 bits of XXH64 of the decompressed content (data frames only).
    pub content_xxh32: u32,
}

/// Which frames the seek table lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeekProfile {
    /// Spec §8.2: every frame except the seek table itself, skippables with Decompressed_Size 0.
    AllFrames,
    /// The Q1 fallback: only Zstandard frames are listed.
    DataOnly,
}

/// Value written in the Checksum column for skippable (zero-size) entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkippableChecksum {
    /// What spec §8.2 currently says.
    Zero,
    /// Low 32 bits of XXH64 of the empty input (0x51D8E999): what a reader that
    /// verified every entry would compute.
    EmptyInputHash,
    /// Any other value, to show the column is never consulted for zero-size entries.
    Custom(u32),
}

#[derive(Clone, Debug)]
pub struct WriterOptions {
    pub profile: SeekProfile,
    pub seek_checksums: bool,
    pub skippable_checksum: SkippableChecksum,
    /// Solid-group target in tar-stream bytes (§12.1).
    pub solid_target: usize,
    /// Split size for entries larger than `solid_target`. Must be a multiple of 512.
    pub chunk: usize,
    /// Extra skippable frames after the Archive Header, e.g. `(MAGIC_DICT, payload)`.
    pub leading_frames: Vec<(u32, Vec<u8>)>,
    /// Insert a vendor skippable frame after every N data frames (spec Q7).
    pub vendor_every: Option<usize>,
    pub creator: String,
}

impl Default for WriterOptions {
    fn default() -> Self {
        Self {
            profile: SeekProfile::AllFrames,
            seek_checksums: true,
            skippable_checksum: SkippableChecksum::Zero,
            solid_target: 4 << 20,
            chunk: 32 << 20,
            leading_frames: Vec::new(),
            vendor_every: None,
            creator: "zsta-seekable-probe".into(),
        }
    }
}

pub struct Archive {
    pub bytes: Vec<u8>,
    /// Every frame in file order, excluding the trailing seek table.
    pub frames: Vec<FrameRecord>,
    /// Seek-table order: `listed[i]` is the index into `frames` of seek-table entry `i`.
    pub listed: Vec<usize>,
    /// Concatenated output of all data frames, including the Terminator.
    pub tar_stream: Vec<u8>,
    pub entries: Vec<TarEntry>,
    pub options: WriterOptions,
}

impl Archive {
    pub fn seek_table_offset(&self) -> u64 {
        self.frames.last().map_or(0, |f| f.file_offset + f.csize)
    }
}

/// §5.3 placement: entries whose extent is at most `solid` share solid frames;
/// larger entries get their own frames, split every `chunk` bytes.
pub fn plan_frames(
    body_len: usize,
    entries: &[TarEntry],
    solid: usize,
    chunk: usize,
) -> Vec<Range<usize>> {
    assert!(
        chunk > 0 && chunk % 512 == 0,
        "chunk must be a positive multiple of 512"
    );
    let mut ranges = Vec::new();
    let (mut group_start, mut cur) = (0usize, 0usize);
    for (i, e) in entries.iter().enumerate() {
        let start = e.header_offset as usize;
        let end = entries
            .get(i + 1)
            .map_or(body_len, |n| n.header_offset as usize);
        assert_eq!(start, cur, "entries must be contiguous");
        let extent = end - start;
        if extent > solid {
            if cur > group_start {
                ranges.push(group_start..cur);
            }
            let mut s = start;
            while s < end {
                let e2 = (s + chunk).min(end);
                ranges.push(s..e2);
                s = e2;
            }
            group_start = end;
        } else if cur > group_start && cur - group_start + extent > solid {
            ranges.push(group_start..cur);
            group_start = cur;
        }
        cur = end;
    }
    if cur > group_start {
        ranges.push(group_start..cur);
    }
    ranges
}

struct Out {
    bytes: Vec<u8>,
    frames: Vec<FrameRecord>,
    stream_offset: u64,
}

impl Out {
    fn skippable(&mut self, role: Role, magic: u32, payload: &[u8]) {
        let file_offset = self.bytes.len() as u64;
        self.bytes.extend_from_slice(&magic.to_le_bytes());
        self.bytes
            .extend_from_slice(&(payload.len() as u32).to_le_bytes());
        self.bytes.extend_from_slice(payload);
        self.frames.push(FrameRecord {
            role,
            file_offset,
            csize: 8 + payload.len() as u64,
            stream_offset: self.stream_offset,
            dsize: 0,
            content_xxh32: 0,
        });
    }

    fn data(
        &mut self,
        role: Role,
        content: &[u8],
        enc: &mut dyn FrameEncoder,
    ) -> Result<(), String> {
        let frame = enc.encode_frame(content)?;
        let file_offset = self.bytes.len() as u64;
        self.bytes.extend_from_slice(&frame);
        self.frames.push(FrameRecord {
            role,
            file_offset,
            csize: frame.len() as u64,
            stream_offset: self.stream_offset,
            dsize: content.len() as u64,
            content_xxh32: xxh64(content, 0) as u32,
        });
        self.stream_offset += content.len() as u64;
        Ok(())
    }
}

fn header_payload(creator: &str) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(b"ZSTA");
    p.push(0); // Major_Version (spec draft 0.2)
    p.push(2); // Minor_Version
    p.extend_from_slice(&0u16.to_le_bytes()); // Archive_Flags
    p.extend_from_slice(&1u16.to_le_bytes()); // TLV: Creator
    p.extend_from_slice(&(creator.len() as u16).to_le_bytes());
    p.extend_from_slice(creator.as_bytes());
    p
}

pub fn write(
    body: &[u8],
    entries: &[TarEntry],
    enc: &mut dyn FrameEncoder,
    opts: &WriterOptions,
) -> Result<Archive, String> {
    assert_eq!(body.len() % 512, 0, "tar body must be block aligned");
    let mut w = Out {
        bytes: Vec::new(),
        frames: Vec::new(),
        stream_offset: 0,
    };

    w.skippable(Role::Header, MAGIC_HEADER, &header_payload(&opts.creator));
    for (magic, payload) in &opts.leading_frames {
        let role = if *magic == MAGIC_DICT {
            Role::Dictionary
        } else {
            Role::Vendor
        };
        w.skippable(role, *magic, payload);
    }
    for (k, r) in plan_frames(body.len(), entries, opts.solid_target, opts.chunk)
        .into_iter()
        .enumerate()
    {
        w.data(Role::Data, &body[r], enc)?;
        if opts.vendor_every.is_some_and(|n| (k + 1) % n == 0) {
            let mut p = b"TEST".to_vec();
            p.extend_from_slice(&(k as u32).to_le_bytes());
            w.skippable(Role::Vendor, MAGIC_VENDOR, &p);
        }
    }
    w.data(Role::Terminator, &[0u8; TERMINATOR_LEN], enc)?;

    let mut tar_stream = body.to_vec();
    tar_stream.resize(body.len() + TERMINATOR_LEN, 0);
    let idx = index::encode(entries, tar_stream.len() as u64, enc)?;
    w.skippable(Role::Index, MAGIC_INDEX, &idx);

    let listed: Vec<usize> = (0..w.frames.len())
        .filter(|&i| opts.profile == SeekProfile::AllFrames || w.frames[i].role.is_zstd())
        .collect();

    let mut st = Vec::new();
    for &i in &listed {
        let f = &w.frames[i];
        let csize = u32::try_from(f.csize).map_err(|_| "frame larger than 4 GiB")?;
        let dsize = u32::try_from(f.dsize).map_err(|_| "frame content larger than 4 GiB")?;
        st.extend_from_slice(&csize.to_le_bytes());
        st.extend_from_slice(&dsize.to_le_bytes());
        if opts.seek_checksums {
            let c = if f.role.is_zstd() {
                f.content_xxh32
            } else {
                match opts.skippable_checksum {
                    SkippableChecksum::Zero => 0,
                    SkippableChecksum::EmptyInputHash => xxh64(&[], 0) as u32,
                    SkippableChecksum::Custom(v) => v,
                }
            };
            st.extend_from_slice(&c.to_le_bytes());
        }
    }
    st.extend_from_slice(&(listed.len() as u32).to_le_bytes());
    st.push(if opts.seek_checksums { 0x80 } else { 0 });
    st.extend_from_slice(&SEEKABLE_FOOTER_MAGIC.to_le_bytes());
    w.bytes.extend_from_slice(&MAGIC_SEEK_TABLE.to_le_bytes());
    w.bytes.extend_from_slice(&(st.len() as u32).to_le_bytes());
    w.bytes.extend_from_slice(&st);

    Ok(Archive {
        bytes: w.bytes,
        frames: w.frames,
        listed,
        tar_stream,
        entries: entries.to_vec(),
        options: opts.clone(),
    })
}
