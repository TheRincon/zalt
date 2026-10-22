//! Safe wrapper over zstd's *reference* seekable reader
//! (`contrib/seekable_format/zstdseek_decompress.c`, compiled by zstd-sys).

use std::marker::PhantomData;

use crate::{error_name, is_error};
use zstd_sys as z;

pub struct RefSeekable<'a> {
    zs: *mut z::ZSTD_seekable,
    _src: PhantomData<&'a [u8]>,
}

impl<'a> RefSeekable<'a> {
    /// `ZSTD_seekable_initBuff`: parse the seek table at the end of `src`.
    pub fn open(src: &'a [u8]) -> Result<Self, String> {
        // SAFETY: zs is checked for NULL; src outlives Self via the lifetime.
        unsafe {
            let zs = z::ZSTD_seekable_create();
            if zs.is_null() {
                return Err("ZSTD_seekable_create failed".into());
            }
            let r = z::ZSTD_seekable_initBuff(zs, src.as_ptr().cast(), src.len());
            if is_error(r) {
                z::ZSTD_seekable_free(zs);
                return Err(format!("ZSTD_seekable_initBuff: {}", error_name(r)));
            }
            Ok(Self {
                zs,
                _src: PhantomData,
            })
        }
    }

    pub fn num_frames(&self) -> u32 {
        unsafe { z::ZSTD_seekable_getNumFrames(self.zs) }
    }

    pub fn frame_c_offset(&self, i: u32) -> u64 {
        unsafe { z::ZSTD_seekable_getFrameCompressedOffset(self.zs, i) }
    }

    pub fn frame_c_size(&self, i: u32) -> u64 {
        unsafe { z::ZSTD_seekable_getFrameCompressedSize(self.zs, i) as u64 }
    }

    pub fn frame_d_offset(&self, i: u32) -> u64 {
        unsafe { z::ZSTD_seekable_getFrameDecompressedOffset(self.zs, i) }
    }

    pub fn frame_d_size(&self, i: u32) -> u64 {
        unsafe { z::ZSTD_seekable_getFrameDecompressedSize(self.zs, i) as u64 }
    }

    pub fn offset_to_frame(&self, off: u64) -> u32 {
        unsafe { z::ZSTD_seekable_offsetToFrameIndex(self.zs, off) }
    }

    /// Total decompressed length as the reference reader sees it.
    pub fn stream_len(&self) -> u64 {
        match self.num_frames() {
            0 => 0,
            n => self.frame_d_offset(n - 1) + self.frame_d_size(n - 1),
        }
    }

    /// `ZSTD_seekable_decompress`: random-access read of the tar stream.
    pub fn read_at(&mut self, off: u64, len: usize) -> Result<Vec<u8>, String> {
        let mut v = vec![0u8; len];
        let r = unsafe { z::ZSTD_seekable_decompress(self.zs, v.as_mut_ptr().cast(), len, off) };
        if is_error(r) {
            return Err(format!(
                "ZSTD_seekable_decompress({off}, {len}): {}",
                error_name(r)
            ));
        }
        v.truncate(r);
        Ok(v)
    }

    /// `ZSTD_seekable_decompressFrame` on seek-table entry `i`.
    pub fn decompress_frame(&mut self, i: u32) -> Result<Vec<u8>, String> {
        let size = self.frame_d_size(i) as usize;
        let mut v = vec![0u8; size.max(1)];
        let r =
            unsafe { z::ZSTD_seekable_decompressFrame(self.zs, v.as_mut_ptr().cast(), v.len(), i) };
        if is_error(r) {
            return Err(format!(
                "ZSTD_seekable_decompressFrame({i}): {}",
                error_name(r)
            ));
        }
        v.truncate(r);
        Ok(v)
    }
}

impl Drop for RefSeekable<'_> {
    fn drop(&mut self) {
        unsafe { z::ZSTD_seekable_free(self.zs) };
    }
}
