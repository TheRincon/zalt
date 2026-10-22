//! What a stock decoder does with a ZLTA file: stream-decode it from the start,
//! exactly like `zstd -dc`, with libzstd's default memory limits.

use crate::{error_name, is_error};
use zstd_sys as z;

/// Decode every frame in `file`, skipping skippable frames. Fails if the input
/// ends mid-frame, as `zstd -t` would.
pub fn decompress_stream(file: &[u8]) -> Result<Vec<u8>, String> {
    // SAFETY: standard streaming loop; buffers outlive every call.
    unsafe {
        let dctx = z::ZSTD_createDCtx();
        if dctx.is_null() {
            return Err("ZSTD_createDCtx failed".into());
        }
        let mut out = Vec::new();
        let mut tmp = vec![0u8; 1 << 17];
        let mut input = z::ZSTD_inBuffer {
            src: file.as_ptr().cast(),
            size: file.len(),
            pos: 0,
        };
        let result = loop {
            let mut o = z::ZSTD_outBuffer {
                dst: tmp.as_mut_ptr().cast(),
                size: tmp.len(),
                pos: 0,
            };
            let r = z::ZSTD_decompressStream(dctx, &mut o, &mut input);
            if is_error(r) {
                break Err(format!(
                    "ZSTD_decompressStream at input {}: {}",
                    input.pos,
                    error_name(r)
                ));
            }
            out.extend_from_slice(&tmp[..o.pos]);
            if input.pos == input.size && o.pos < o.size {
                break if r == 0 {
                    Ok(())
                } else {
                    Err("input ends mid-frame".into())
                };
            }
        };
        z::ZSTD_freeDCtx(dctx);
        result.map(|()| out)
    }
}
