//! C ABI for zalt.
//!
//! This is the surface a Mac app links against in-process — from Swift,
//! Objective-C, C or C++. Everything here exists to make the boundary safe in
//! the direction that matters: **nothing that goes wrong inside zalt may become
//! the host's problem.**
//!
//! # Rules this boundary keeps
//!
//! **No panic escapes.** Every entry point is wrapped in `catch_unwind`.
//! Unwinding across an FFI boundary is undefined behaviour, and aborting would
//! let a malformed subresource take down a browser's network process when the
//! right outcome is to fail that one resource. Parsing is `forbid(unsafe_code)`
//! and no panic should be reachable from it — but "should" is load-bearing in
//! that sentence, and this is where a wrong assumption stops being ours.
//!
//! **No Rust allocations cross the boundary.** Output goes into a
//! caller-provided buffer. The host never has to call back into us to free
//! something, and can account for every byte.
//!
//! **Error codes are ABI.** They come from [`zalt_codec::Error::code`], which
//! assigns them explicitly rather than by declaration order, so reordering a
//! Rust enum cannot silently change what a C caller reads.
//!
//! **Every pointer is checked.** A null handle or buffer returns an error code,
//! never a dereference.
//!
//! # Safety
//!
//! This crate contains `unsafe` because a C ABI cannot exist without it. It is
//! the second of exactly two crates permitted to (`zalt-neon` is the other),
//! and the unsafety is confined to pointer validation at the entry points.

#![deny(missing_docs)]

use std::ffi::{c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::slice;

use zalt_codec::Error as CodecError;
use zalt_codec::decode::Status;
use zalt_codec::params::DecodeLimits;
use zalt_embed::{Decoder, Policy};

// ----------------------------------------------------------------- codes

/// Success.
pub const ZALT_OK: i32 = 0;

/// A null pointer was passed where one is required.
pub const ZALT_ERR_NULL_POINTER: i32 = -1;
/// The output buffer was too small to make progress.
pub const ZALT_ERR_OUTPUT_TOO_SMALL: i32 = -2;
/// The decoder needs more memory than its budget allows.
pub const ZALT_ERR_OVER_BUDGET: i32 = -3;
/// A panic was caught at the boundary. Always a bug in zalt; report it.
pub const ZALT_ERR_PANIC: i32 = -4;
/// An argument was outside its permitted range.
pub const ZALT_ERR_BAD_ARGUMENT: i32 = -5;

/// The decoder consumed all input and needs more.
pub const ZALT_STATUS_NEED_MORE_INPUT: i32 = 0;
/// The output buffer filled. Call again with space; no new input is needed.
pub const ZALT_STATUS_HAS_MORE_OUTPUT: i32 = 1;
/// A frame finished and all of its output has been delivered.
pub const ZALT_STATUS_FRAME_COMPLETE: i32 = 2;

/// ABI version. Bumped on any incompatible change to this header.
pub const ZALT_ABI_VERSION: u32 = 1;

// ---------------------------------------------------------------- handle

/// Opaque decoder handle.
///
/// Created by [`zalt_decoder_new`], destroyed by [`zalt_decoder_free`]. Not
/// thread-safe: one handle belongs to one thread at a time. Concurrency is the
/// host's to arrange, which is deliberate — a browser already has a threading
/// model and does not need ours.
#[allow(
    non_camel_case_types,
    reason = "the C header uses snake_case type names"
)]
pub struct zalt_decoder {
    decoder: Decoder,
    /// Decoded bytes not yet handed to the caller.
    pending: Vec<u8>,
    /// How much of `pending` has been delivered.
    delivered: usize,
    /// A frame completed and is waiting for its output to drain.
    frame_done: bool,
}

/// Run `f`, converting a panic into [`ZALT_ERR_PANIC`].
fn guard<F: FnOnce() -> i32>(f: F) -> i32 {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(ZALT_ERR_PANIC)
}

/// Map a codec error to its ABI code, negated so success and status values
/// stay non-negative.
fn code_of(e: CodecError) -> i32 {
    -(e.code() as i32) - 100
}

/// Recover a codec error code from the ABI representation.
fn is_codec_code(code: i32) -> bool {
    code <= -100
}

// ------------------------------------------------------------ lifecycle

/// The ABI version this library implements.
#[unsafe(no_mangle)]
pub extern "C" fn zalt_abi_version() -> u32 {
    ZALT_ABI_VERSION
}

/// Library version string, NUL-terminated and statically allocated.
#[unsafe(no_mangle)]
pub extern "C" fn zalt_version_string() -> *const c_char {
    concat!("zalt ", env!("CARGO_PKG_VERSION"), "\0")
        .as_ptr()
        .cast()
}

/// Name of the acceleration backend this build selected.
#[unsafe(no_mangle)]
pub extern "C" fn zalt_backend_name() -> *const c_char {
    #[cfg(target_arch = "aarch64")]
    {
        c"neon".as_ptr()
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        c"scalar".as_ptr()
    }
}

/// Create a decoder.
///
/// `max_window_log` bounds the window a frame may declare; pass 23 for the
/// 8 MiB ceiling appropriate to untrusted network input, or 0 for the library
/// default. `max_decoded_size` bounds total output; pass 0 for no limit.
///
/// Returns NULL on allocation failure or an out-of-range argument.
#[unsafe(no_mangle)]
pub extern "C" fn zalt_decoder_new(max_window_log: u8, max_decoded_size: u64) -> *mut zalt_decoder {
    catch_unwind(|| {
        let mut policy = Policy::untrusted();
        if max_window_log != 0 {
            if !(10..=31).contains(&max_window_log) {
                return ptr::null_mut();
            }
            policy.limits.max_window_log = max_window_log;
        }
        policy.limits.max_decoded_size = (max_decoded_size != 0).then_some(max_decoded_size);

        Box::into_raw(Box::new(zalt_decoder {
            decoder: Decoder::new(policy),
            pending: Vec::new(),
            delivered: 0,
            frame_done: false,
        }))
    })
    .unwrap_or(ptr::null_mut())
}

/// Destroy a decoder. Passing NULL is allowed and does nothing.
///
/// # Safety
///
/// `handle` must have come from [`zalt_decoder_new`] and must not be used
/// again afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zalt_decoder_free(handle: *mut zalt_decoder) {
    if handle.is_null() {
        return;
    }
    // A panic while dropping would unwind into C. Swallow it: the handle is
    // gone either way, and there is nobody left to report to.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        drop(unsafe { Box::from_raw(handle) });
    }));
}

/// Reset a decoder for an unrelated stream, keeping its allocations.
///
/// # Safety
///
/// `handle` must be a live handle from [`zalt_decoder_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zalt_decoder_reset(handle: *mut zalt_decoder) -> i32 {
    guard(|| {
        let Some(h) = (unsafe { handle.as_mut() }) else {
            return ZALT_ERR_NULL_POINTER;
        };
        h.decoder.reset();
        h.pending.clear();
        h.delivered = 0;
        h.frame_done = false;
        ZALT_OK
    })
}

/// Bytes of heap the decoder currently holds. Returns 0 for a NULL handle.
///
/// # Safety
///
/// `handle` must be a live handle or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zalt_decoder_footprint(handle: *const zalt_decoder) -> usize {
    catch_unwind(AssertUnwindSafe(|| match unsafe { handle.as_ref() } {
        Some(h) => h.decoder.footprint() + h.pending.capacity(),
        None => 0,
    }))
    .unwrap_or(0)
}

/// Release the decoder's buffers, keeping it usable.
///
/// # Safety
///
/// `handle` must be a live handle from [`zalt_decoder_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zalt_decoder_shrink(handle: *mut zalt_decoder) -> i32 {
    guard(|| {
        let Some(h) = (unsafe { handle.as_mut() }) else {
            return ZALT_ERR_NULL_POINTER;
        };
        h.decoder.shrink();
        h.pending = Vec::new();
        h.delivered = 0;
        h.frame_done = false;
        ZALT_OK
    })
}

// ------------------------------------------------------------ streaming

impl zalt_decoder {
    /// Move as much of `pending` into `out` as fits.
    fn deliver(&mut self, out: &mut [u8]) -> usize {
        let available = self.pending.len() - self.delivered;
        let n = available.min(out.len());
        out[..n].copy_from_slice(&self.pending[self.delivered..self.delivered + n]);
        self.delivered += n;
        if self.delivered == self.pending.len() {
            self.pending.clear();
            self.delivered = 0;
        }
        n
    }

    fn has_pending(&self) -> bool {
        self.delivered < self.pending.len()
    }
}

/// Decode incrementally.
///
/// Consumes from `input` and writes to `output`, reporting how much of each was
/// used and what it needs next via `status`. Suspends at any byte boundary, so
/// the caller may feed socket-sized chunks and drain output as it likes.
///
/// A frame is only reported [`ZALT_STATUS_FRAME_COMPLETE`] once all of its
/// output has been delivered — a full output buffer yields
/// [`ZALT_STATUS_HAS_MORE_OUTPUT`] instead, and the caller should call again
/// with space and no new input.
///
/// Returns [`ZALT_OK`] or a negative error code.
///
/// # Safety
///
/// `handle` must be live. `input` must be valid for `input_len` bytes and
/// `output` for `output_cap` bytes. The out-parameters must be valid pointers
/// or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zalt_decoder_decompress(
    handle: *mut zalt_decoder,
    input: *const u8,
    input_len: usize,
    input_consumed: *mut usize,
    output: *mut u8,
    output_cap: usize,
    output_written: *mut usize,
    status: *mut i32,
) -> i32 {
    guard(|| {
        let Some(h) = (unsafe { handle.as_mut() }) else {
            return ZALT_ERR_NULL_POINTER;
        };
        if (input.is_null() && input_len != 0) || (output.is_null() && output_cap != 0) {
            return ZALT_ERR_NULL_POINTER;
        }

        let in_slice: &[u8] = if input_len == 0 {
            &[]
        } else {
            unsafe { slice::from_raw_parts(input, input_len) }
        };
        let out_slice: &mut [u8] = if output_cap == 0 {
            &mut []
        } else {
            unsafe { slice::from_raw_parts_mut(output, output_cap) }
        };

        let mut written = 0usize;
        let mut consumed = 0usize;

        // Deliver whatever is already decoded before asking for more.
        written += h.deliver(&mut out_slice[written..]);

        let result_status = if h.has_pending() {
            ZALT_STATUS_HAS_MORE_OUTPUT
        } else if h.frame_done {
            h.frame_done = false;
            ZALT_STATUS_FRAME_COMPLETE
        } else {
            // Pending is drained; decode more.
            match h.decoder.decompress(in_slice, &mut h.pending) {
                Ok(progress) => {
                    consumed = progress.consumed;
                    if progress.status == Status::FrameComplete {
                        h.frame_done = true;
                    }
                    written += h.deliver(&mut out_slice[written..]);
                    if h.has_pending() {
                        ZALT_STATUS_HAS_MORE_OUTPUT
                    } else if h.frame_done {
                        h.frame_done = false;
                        ZALT_STATUS_FRAME_COMPLETE
                    } else {
                        ZALT_STATUS_NEED_MORE_INPUT
                    }
                }
                Err(zalt_embed::Error::Codec(e)) => return code_of(e),
                Err(zalt_embed::Error::OverBudget { .. }) => return ZALT_ERR_OVER_BUDGET,
                // `zalt_embed::Error` is non_exhaustive. A variant added later
                // must surface as *some* error here rather than failing to
                // compile a shipped ABI, so it maps to the generic code.
                Err(_) => return ZALT_ERR_BAD_ARGUMENT,
            }
        };

        if !input_consumed.is_null() {
            unsafe { *input_consumed = consumed };
        }
        if !output_written.is_null() {
            unsafe { *output_written = written };
        }
        if !status.is_null() {
            unsafe { *status = result_status };
        }
        ZALT_OK
    })
}

/// Decode a complete frame into a caller-provided buffer.
///
/// Sets `output_written` to the decoded size on success. Returns
/// [`ZALT_ERR_OUTPUT_TOO_SMALL`] when `output_cap` is insufficient; the frame's
/// declared content size, when it has one, is available from
/// [`zalt_frame_content_size`] beforehand.
///
/// # Safety
///
/// `input` must be valid for `input_len` bytes, `output` for `output_cap`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zalt_decompress(
    input: *const u8,
    input_len: usize,
    max_window_log: u8,
    output: *mut u8,
    output_cap: usize,
    output_written: *mut usize,
) -> i32 {
    guard(|| {
        if input.is_null() || (output.is_null() && output_cap != 0) {
            return ZALT_ERR_NULL_POINTER;
        }
        let in_slice = unsafe { slice::from_raw_parts(input, input_len) };

        let mut limits = DecodeLimits {
            max_window_log: if max_window_log == 0 {
                23
            } else {
                max_window_log
            },
            max_decoded_size: Some(output_cap as u64),
        };
        if !(10..=31).contains(&limits.max_window_log) {
            return ZALT_ERR_BAD_ARGUMENT;
        }
        // Bounding decoded size by the caller's buffer means an oversized frame
        // is refused during decoding rather than after filling memory we would
        // then have to throw away.
        if output_cap == 0 {
            limits.max_decoded_size = Some(0);
        }

        match zalt_codec::decompress(in_slice, limits) {
            Ok(decoded) => {
                if decoded.len() > output_cap {
                    return ZALT_ERR_OUTPUT_TOO_SMALL;
                }
                if !decoded.is_empty() {
                    unsafe {
                        ptr::copy_nonoverlapping(decoded.as_ptr(), output, decoded.len());
                    }
                }
                if !output_written.is_null() {
                    unsafe { *output_written = decoded.len() };
                }
                ZALT_OK
            }
            Err(CodecError::TooLarge) => ZALT_ERR_OUTPUT_TOO_SMALL,
            Err(e) => code_of(e),
        }
    })
}

/// Read a frame's declared content size without decoding it.
///
/// Writes the size to `out` and returns [`ZALT_OK`]. Returns
/// [`ZALT_ERR_BAD_ARGUMENT`] when the frame declares no size, which is legal —
/// a streaming encoder need not know it in advance.
///
/// # Safety
///
/// `input` must be valid for `input_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zalt_frame_content_size(
    input: *const u8,
    input_len: usize,
    out: *mut u64,
) -> i32 {
    guard(|| {
        if input.is_null() || out.is_null() {
            return ZALT_ERR_NULL_POINTER;
        }
        let data = unsafe { slice::from_raw_parts(input, input_len) };
        if data.len() < 5 {
            return code_of(CodecError::Truncated);
        }
        let magic = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        if magic != zalt_codec::magic::ZSTD_FRAME {
            return code_of(CodecError::BadMagic);
        }
        let descriptor = data[4];
        let Ok(need) = zalt_codec::frame::header_len_after_descriptor(descriptor) else {
            return code_of(CodecError::Reserved);
        };
        let Some(rest) = data.get(5..5 + need) else {
            return code_of(CodecError::Truncated);
        };
        // Permissive limits: this only reads a declared number, and refusing
        // here on policy grounds would hide the size the caller is asking for.
        match zalt_codec::FrameHeader::parse(descriptor, rest, &DecodeLimits::default()) {
            Ok(header) => match header.frame_content_size {
                Some(size) => {
                    unsafe { *out = size };
                    ZALT_OK
                }
                None => ZALT_ERR_BAD_ARGUMENT,
            },
            Err(e) => code_of(e),
        }
    })
}

// -------------------------------------------------------------- errors

/// A human-readable, statically allocated message for a code.
#[unsafe(no_mangle)]
pub extern "C" fn zalt_error_message(code: i32) -> *const c_char {
    let msg = match code {
        ZALT_OK => c"ok",
        ZALT_ERR_NULL_POINTER => c"null pointer",
        ZALT_ERR_OUTPUT_TOO_SMALL => c"output buffer too small",
        ZALT_ERR_OVER_BUDGET => c"decoder exceeded its memory budget",
        ZALT_ERR_PANIC => c"internal error (panic caught at the boundary)",
        ZALT_ERR_BAD_ARGUMENT => c"argument out of range",
        _ if is_codec_code(code) => match -(code + 100) {
            1 => c"not a modern zstd frame",
            2 => c"reserved bit set",
            3 => c"frame requests a window larger than permitted",
            4 => c"match offset exceeds available history",
            5 => c"malformed input",
            6 => c"reuse of an entropy table that was never set",
            7 => c"decoded size exceeds limit",
            8 => c"content checksum mismatch",
            9 => c"truncated input",
            10 => c"frame requires a dictionary",
            11 => c"feature not implemented",
            _ => c"unknown error",
        },
        _ => c"unknown error",
    };
    msg.as_ptr()
}

/// Whether a caller might succeed by acting on this error rather than failing
/// the resource — fetching a dictionary, or retrying without `zstd` in
/// `Accept-Encoding`.
#[unsafe(no_mangle)]
pub extern "C" fn zalt_error_is_recoverable(code: i32) -> bool {
    if !is_codec_code(code) {
        return code == ZALT_ERR_OVER_BUDGET || code == ZALT_ERR_OUTPUT_TOO_SMALL;
    }
    matches!(-(code + 100), 3 | 10 | 11)
}

/// Unused; present so cbindgen emits `void *` in the header for opaque use.
#[doc(hidden)]
pub type ZaltReserved = *mut c_void;
