//! The C ABI exercised the way a C caller drives it.
//!
//! These go through the `extern "C"` functions with raw pointers rather than
//! any Rust-side convenience, because the boundary is the thing under test.
//! In particular: a decoder is driven with a **deliberately small output
//! buffer**, which is the case a host actually hits and the one where the
//! `HAS_MORE_OUTPUT` handshake either works or silently loses bytes.

use std::ptr;

use zalt::*;

const HELLO_ZST: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x24, 0x0b, 0x59, 0x00, 0x00, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x20, 0x77,
    0x6f, 0x72, 0x6c, 0x64, 0x68, 0x69, 0x1e, 0xb2,
];

fn cstr(p: *const std::ffi::c_char) -> String {
    assert!(!p.is_null());
    unsafe { std::ffi::CStr::from_ptr(p) }
        .to_string_lossy()
        .into_owned()
}

#[test]
fn version_and_backend_are_reportable() {
    assert_eq!(zalt_abi_version(), ZALT_ABI_VERSION);
    assert!(cstr(zalt_version_string()).starts_with("zalt"));
    let backend = cstr(zalt_backend_name());
    assert!(backend == "neon" || backend == "scalar", "{backend}");
}

#[test]
fn one_shot_decode() {
    let mut out = vec![0u8; 64];
    let mut written = 0usize;
    let rc = unsafe {
        zalt_decompress(
            HELLO_ZST.as_ptr(),
            HELLO_ZST.len(),
            23,
            out.as_mut_ptr(),
            out.len(),
            &mut written,
        )
    };
    assert_eq!(rc, ZALT_OK, "{}", cstr(zalt_error_message(rc)));
    assert_eq!(&out[..written], b"hello world");
}

#[test]
fn one_shot_reports_a_too_small_buffer() {
    let mut out = vec![0u8; 4];
    let mut written = 0usize;
    let rc = unsafe {
        zalt_decompress(
            HELLO_ZST.as_ptr(),
            HELLO_ZST.len(),
            23,
            out.as_mut_ptr(),
            out.len(),
            &mut written,
        )
    };
    assert_eq!(rc, ZALT_ERR_OUTPUT_TOO_SMALL);
    assert!(zalt_error_is_recoverable(rc), "the caller can retry bigger");
}

/// **The handshake that matters.** A one-byte output buffer forces the
/// `HAS_MORE_OUTPUT` path on every single byte, so a decoder that loses or
/// duplicates output when its buffer fills cannot pass this.
#[test]
fn streaming_through_a_one_byte_output_buffer() {
    let dec = zalt_decoder_new(23, 0);
    assert!(!dec.is_null());

    let mut collected = Vec::new();
    let mut cursor = 0usize;
    let mut byte = [0u8; 1];
    let mut saw_complete = false;

    for _ in 0..10_000 {
        let mut consumed = 0usize;
        let mut written = 0usize;
        let mut status = -1i32;
        let rc = unsafe {
            zalt_decoder_decompress(
                dec,
                HELLO_ZST[cursor..].as_ptr(),
                HELLO_ZST.len() - cursor,
                &mut consumed,
                byte.as_mut_ptr(),
                1,
                &mut written,
                &mut status,
            )
        };
        assert_eq!(rc, ZALT_OK, "{}", cstr(zalt_error_message(rc)));
        cursor += consumed;
        collected.extend_from_slice(&byte[..written]);

        if status == ZALT_STATUS_FRAME_COMPLETE {
            saw_complete = true;
            break;
        }
        if status == ZALT_STATUS_NEED_MORE_INPUT && cursor >= HELLO_ZST.len() && written == 0 {
            break;
        }
    }

    assert!(saw_complete, "frame never reported complete");
    assert_eq!(collected, b"hello world");
    unsafe { zalt_decoder_free(dec) };
}

#[test]
fn streaming_with_one_byte_input_chunks() {
    let dec = zalt_decoder_new(23, 0);
    let mut collected = Vec::new();
    let mut out = vec![0u8; 256];
    let mut cursor = 0usize;

    while cursor < HELLO_ZST.len() {
        let mut consumed = 0usize;
        let mut written = 0usize;
        let mut status = -1i32;
        let rc = unsafe {
            zalt_decoder_decompress(
                dec,
                HELLO_ZST[cursor..].as_ptr(),
                1,
                &mut consumed,
                out.as_mut_ptr(),
                out.len(),
                &mut written,
                &mut status,
            )
        };
        assert_eq!(rc, ZALT_OK);
        cursor += consumed;
        collected.extend_from_slice(&out[..written]);
    }

    assert_eq!(collected, b"hello world");
    unsafe { zalt_decoder_free(dec) };
}

#[test]
fn reset_allows_reuse() {
    let dec = zalt_decoder_new(23, 0);
    let mut out = vec![0u8; 64];

    for _ in 0..3 {
        assert_eq!(unsafe { zalt_decoder_reset(dec) }, ZALT_OK);
        let mut consumed = 0;
        let mut written = 0;
        let mut status = -1;
        let rc = unsafe {
            zalt_decoder_decompress(
                dec,
                HELLO_ZST.as_ptr(),
                HELLO_ZST.len(),
                &mut consumed,
                out.as_mut_ptr(),
                out.len(),
                &mut written,
                &mut status,
            )
        };
        assert_eq!(rc, ZALT_OK);
        assert_eq!(&out[..written], b"hello world");
    }
    unsafe { zalt_decoder_free(dec) };
}

#[test]
fn footprint_is_reportable_and_shrinkable() {
    let dec = zalt_decoder_new(23, 0);
    let mut out = vec![0u8; 64];
    let (mut c, mut w, mut s) = (0usize, 0usize, -1i32);
    unsafe {
        zalt_decoder_decompress(
            dec,
            HELLO_ZST.as_ptr(),
            HELLO_ZST.len(),
            &mut c,
            out.as_mut_ptr(),
            out.len(),
            &mut w,
            &mut s,
        )
    };
    assert!(unsafe { zalt_decoder_footprint(dec) } > 0);
    assert_eq!(unsafe { zalt_decoder_shrink(dec) }, ZALT_OK);
    assert_eq!(unsafe { zalt_decoder_footprint(dec) }, 0);
    unsafe { zalt_decoder_free(dec) };
}

#[test]
fn content_size_is_readable_without_decoding() {
    let mut size = 0u64;
    let rc = unsafe { zalt_frame_content_size(HELLO_ZST.as_ptr(), HELLO_ZST.len(), &mut size) };
    assert_eq!(rc, ZALT_OK);
    assert_eq!(size, 11);
}

// ------------------------------------------------------- hostile callers

/// Every entry point must survive a null pointer with an error code, never a
/// dereference. A host that gets this wrong should get a diagnostic, not a
/// crash in someone else's library.
#[test]
fn null_pointers_are_errors_not_crashes() {
    assert_eq!(
        unsafe { zalt_decoder_reset(ptr::null_mut()) },
        ZALT_ERR_NULL_POINTER
    );
    assert_eq!(
        unsafe { zalt_decoder_shrink(ptr::null_mut()) },
        ZALT_ERR_NULL_POINTER
    );
    assert_eq!(unsafe { zalt_decoder_footprint(ptr::null()) }, 0);

    // Freeing NULL is explicitly allowed.
    unsafe { zalt_decoder_free(ptr::null_mut()) };

    let mut written = 0usize;
    assert_eq!(
        unsafe { zalt_decompress(ptr::null(), 0, 23, ptr::null_mut(), 0, &mut written) },
        ZALT_ERR_NULL_POINTER
    );

    let mut size = 0u64;
    assert_eq!(
        unsafe { zalt_frame_content_size(ptr::null(), 0, &mut size) },
        ZALT_ERR_NULL_POINTER
    );

    let dec = zalt_decoder_new(23, 0);
    let mut c = 0usize;
    let mut w = 0usize;
    let mut s = -1i32;
    assert_eq!(
        unsafe {
            zalt_decoder_decompress(
                dec,
                ptr::null(),
                5,
                &mut c,
                ptr::null_mut(),
                5,
                &mut w,
                &mut s,
            )
        },
        ZALT_ERR_NULL_POINTER
    );
    unsafe { zalt_decoder_free(dec) };
}

/// NULL out-parameters are legal: a caller that does not care about a value
/// should not have to provide storage for it.
#[test]
fn null_out_parameters_are_permitted() {
    let dec = zalt_decoder_new(23, 0);
    let mut out = vec![0u8; 64];
    let rc = unsafe {
        zalt_decoder_decompress(
            dec,
            HELLO_ZST.as_ptr(),
            HELLO_ZST.len(),
            ptr::null_mut(),
            out.as_mut_ptr(),
            out.len(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    assert_eq!(rc, ZALT_OK);
    unsafe { zalt_decoder_free(dec) };
}

#[test]
fn out_of_range_window_log_is_refused() {
    assert!(zalt_decoder_new(9, 0).is_null(), "below the format minimum");
    assert!(zalt_decoder_new(32, 0).is_null(), "above what we support");
    let ok = zalt_decoder_new(23, 0);
    assert!(!ok.is_null());
    unsafe { zalt_decoder_free(ok) };
}

#[test]
fn malformed_input_returns_a_codec_error() {
    let mut out = vec![0u8; 64];
    let mut written = 0usize;
    let garbage = [0u8; 32];
    let rc = unsafe {
        zalt_decompress(
            garbage.as_ptr(),
            garbage.len(),
            23,
            out.as_mut_ptr(),
            out.len(),
            &mut written,
        )
    };
    assert!(rc < 0, "garbage must not decode");
    assert_eq!(cstr(zalt_error_message(rc)), "not a modern zstd frame");
}

/// The window policy must be enforceable from C, since that is the whole
/// reason the parameter exists.
#[test]
fn the_window_cap_is_enforced_across_the_boundary() {
    // windowLog 41 from a one-byte descriptor.
    let frame = [0x28u8, 0xb5, 0x2f, 0xfd, 0x00, 0xff, 0x00, 0x00, 0x00];
    let mut out = vec![0u8; 64];
    let mut written = 0usize;
    let rc = unsafe {
        zalt_decompress(
            frame.as_ptr(),
            frame.len(),
            23,
            out.as_mut_ptr(),
            out.len(),
            &mut written,
        )
    };
    assert_eq!(
        cstr(zalt_error_message(rc)),
        "frame requests a window larger than permitted"
    );
    assert!(
        zalt_error_is_recoverable(rc),
        "the caller may retry without zstd"
    );
}

#[test]
fn every_error_code_has_a_message() {
    for code in [
        ZALT_OK,
        ZALT_ERR_NULL_POINTER,
        ZALT_ERR_OUTPUT_TOO_SMALL,
        ZALT_ERR_OVER_BUDGET,
        ZALT_ERR_PANIC,
        ZALT_ERR_BAD_ARGUMENT,
    ] {
        assert_ne!(
            cstr(zalt_error_message(code)),
            "unknown error",
            "code {code}"
        );
    }
    for codec in 1..=11 {
        let code = -codec - 100;
        assert_ne!(
            cstr(zalt_error_message(code)),
            "unknown error",
            "codec code {codec}"
        );
    }
}
