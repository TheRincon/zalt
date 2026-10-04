//! Probe for Open Question Q1 of the ZSTA archive spec (`zstd-archives.md`).
//!
//! > Q1. Confirm the behavior of `contrib/seekable_format` with a leading skippable
//! > frame and zero-size seek table entries.
//!
//! The crate contains just enough to answer that question with real archives:
//!
//! * [`archive`]  – a minimal ZSTA writer (header, data frames, terminator, central
//!   index, seek table) with switches for the layouts being compared;
//! * [`index`]    – Central Index encode/decode (§7), including the VarInt rules (§2);
//! * [`seekable`] – a safe wrapper over zstd's *reference* seekable reader;
//! * [`stock`]    – a plain streaming decode, i.e. what `zstd -dc` does;
//! * [`frame`]    – frame inspection and the §11.3 recovery scan;
//! * [`tar`], [`corpus`], [`xxh64`] – supporting pieces.
//!
//! To run the suite against your own encoder, implement [`encoder::FrameEncoder`]
//! and return it from `encoder_at()` in `tests/common/mod.rs`.

pub mod archive;
pub mod corpus;
pub mod encoder;
pub mod frame;
pub mod index;
pub mod seekable;
pub mod stock;
pub mod tar;
pub mod xxh64;

pub(crate) fn is_error(code: usize) -> bool {
    // SAFETY: pure function over an integer.
    unsafe { zstd_sys::ZSTD_isError(code) != 0 }
}

pub(crate) fn error_name(code: usize) -> String {
    // SAFETY: ZSTD_getErrorName always returns a static, NUL-terminated string.
    unsafe { std::ffi::CStr::from_ptr(zstd_sys::ZSTD_getErrorName(code)) }
        .to_string_lossy()
        .into_owned()
}

pub(crate) fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}

pub(crate) fn le64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}
