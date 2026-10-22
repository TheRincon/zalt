//! The reference XXH3-128 — xxHash's own C, compiled from `c/` — as the
//! oracle for `zalt-archive`'s implementation.

extern "C" {
    fn zlta_probe_xxh3_128(data: *const u8, len: usize, out: *mut u8);
}

/// XXH3-128 of `data` by the reference implementation, in canonical byte
/// order (high half, then low, each big-endian).
pub fn xxh3_128(data: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    // SAFETY: `data` is valid for `data.len()` bytes and `out` for 16; the C
    // reads the one and writes the other, and keeps neither.
    unsafe { zlta_probe_xxh3_128(data.as_ptr(), data.len(), out.as_mut_ptr()) };
    out
}
