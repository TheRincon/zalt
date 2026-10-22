//! The encoder plug-in point.
//!
//! Everything in the probe that produces a Zstandard frame goes through
//! [`FrameEncoder`]. The default is the reference libzstd encoder; implement the
//! trait for your own encoder to run the whole suite (including the §5.2
//! conformance checks) against it.

use crate::{error_name, is_error};
use zstd_sys as z;

pub trait FrameEncoder {
    /// Compress `src` into exactly one standalone Zstandard frame.
    ///
    /// To satisfy §5.2 the frame must carry `Frame_Content_Size`, should carry a
    /// content checksum, must not reference a dictionary, and must not need a
    /// window larger than 2^27 bytes.
    fn encode_frame(&mut self, src: &[u8]) -> Result<Vec<u8>, String>;

    /// Human-readable description, used in assertion messages.
    fn describe(&self) -> String;
}

/// Reference encoder: libzstd via `ZSTD_compress2`.
pub struct ReferenceEncoder {
    cctx: *mut z::ZSTD_CCtx,
    level: i32,
}

impl ReferenceEncoder {
    pub fn new(level: i32) -> Self {
        // SAFETY: plain allocation; checked for NULL below.
        let cctx = unsafe { z::ZSTD_createCCtx() };
        assert!(!cctx.is_null(), "ZSTD_createCCtx failed");
        let set = |param, value| {
            // SAFETY: cctx is valid; parameters are standard ones.
            let r = unsafe { z::ZSTD_CCtx_setParameter(cctx, param, value) };
            assert!(!is_error(r), "setParameter({param:?}): {}", error_name(r));
        };
        set(z::ZSTD_cParameter::ZSTD_c_compressionLevel, level);
        set(z::ZSTD_cParameter::ZSTD_c_checksumFlag, 1);
        set(z::ZSTD_cParameter::ZSTD_c_contentSizeFlag, 1);
        Self { cctx, level }
    }
}

impl FrameEncoder for ReferenceEncoder {
    fn encode_frame(&mut self, src: &[u8]) -> Result<Vec<u8>, String> {
        // SAFETY: bound computation, then a single-shot compression into a buffer
        // of exactly that capacity.
        unsafe {
            let cap = z::ZSTD_compressBound(src.len());
            let mut dst = vec![0u8; cap];
            let n = z::ZSTD_compress2(
                self.cctx,
                dst.as_mut_ptr().cast(),
                cap,
                src.as_ptr().cast(),
                src.len(),
            );
            if is_error(n) {
                return Err(format!("ZSTD_compress2: {}", error_name(n)));
            }
            dst.truncate(n);
            Ok(dst)
        }
    }

    fn describe(&self) -> String {
        format!("libzstd reference encoder, level {}", self.level)
    }
}

impl Drop for ReferenceEncoder {
    fn drop(&mut self) {
        // SAFETY: cctx came from ZSTD_createCCtx and is freed exactly once.
        unsafe { z::ZSTD_freeCCtx(self.cctx) };
    }
}
