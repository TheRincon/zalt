//! Frame inspection and the §11.3 recovery scan.

use crate::{error_name, is_error, le32};
use zstd_sys as z;

pub const ZSTD_MAGIC: u32 = 0xFD2F_B528;
pub const SKIPPABLE_MAGIC_MIN: u32 = 0x184D_2A50;
pub const SKIPPABLE_MAGIC_MAX: u32 = 0x184D_2A5F;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameKind {
    Zstd {
        content_size: Option<u64>,
        window_size: u64,
        has_checksum: bool,
        dict_id: u32,
    },
    Skippable {
        magic: u32,
        user_size: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannedFrame {
    pub offset: u64,
    pub csize: u64,
    pub kind: FrameKind,
}

/// Inspect the frame that starts at `src[0]`. Returns its kind and on-disk size.
pub fn inspect(src: &[u8]) -> Result<(FrameKind, u64), String> {
    if src.len() < 8 {
        return Err("truncated frame".into());
    }
    // SAFETY: reads at most src.len() bytes.
    let csize = unsafe { z::ZSTD_findFrameCompressedSize(src.as_ptr().cast(), src.len()) };
    if is_error(csize) {
        return Err(format!(
            "ZSTD_findFrameCompressedSize: {}",
            error_name(csize)
        ));
    }
    let magic = le32(src);
    if (SKIPPABLE_MAGIC_MIN..=SKIPPABLE_MAGIC_MAX).contains(&magic) {
        let user_size = le32(&src[4..]);
        return Ok((FrameKind::Skippable { magic, user_size }, csize as u64));
    }
    if magic != ZSTD_MAGIC {
        return Err(format!("not a Zstandard frame (magic {magic:#010x})"));
    }
    // SAFETY: zeroed POD header struct, filled by libzstd from at most src.len() bytes.
    let mut h: z::ZSTD_FrameHeader = unsafe { std::mem::zeroed() };
    let r = unsafe { z::ZSTD_getFrameHeader(&mut h, src.as_ptr().cast(), src.len()) };
    if r != 0 {
        return Err(if is_error(r) {
            format!("ZSTD_getFrameHeader: {}", error_name(r))
        } else {
            "truncated frame header".into()
        });
    }
    let unknown = z::ZSTD_CONTENTSIZE_UNKNOWN as u64;
    Ok((
        FrameKind::Zstd {
            content_size: (h.frameContentSize != unknown).then_some(h.frameContentSize),
            window_size: h.windowSize,
            has_checksum: h.checksumFlag != 0,
            dict_id: h.dictID,
        },
        csize as u64,
    ))
}

/// Walk the whole file frame by frame, the way §11.3 rebuilds a lost trailer.
pub fn scan(file: &[u8]) -> Result<Vec<ScannedFrame>, String> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off < file.len() {
        let (kind, csize) =
            inspect(&file[off..]).map_err(|e| format!("at file offset {off}: {e}"))?;
        out.push(ScannedFrame {
            offset: off as u64,
            csize,
            kind,
        });
        off += csize as usize;
    }
    Ok(out)
}
