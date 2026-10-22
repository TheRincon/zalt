//! Pure-Rust XXH64, used for seek-table checksums (§8.2) and the Index_Checksum (§7).
//!
//! Kept dependency-free on purpose. It is cross-checked against the checksums that
//! libzstd itself writes into frames (see `tests/q1_seekable.rs`).

const P1: u64 = 0x9E37_79B1_85EB_CA87;
const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const P3: u64 = 0x1656_67B1_9E37_79F9;
const P4: u64 = 0x85EB_CA77_C2B2_AE63;
const P5: u64 = 0x27D4_EB2F_1656_67C5;

#[inline]
fn round(acc: u64, lane: u64) -> u64 {
    acc.wrapping_add(lane.wrapping_mul(P2))
        .rotate_left(31)
        .wrapping_mul(P1)
}

#[inline]
fn merge(acc: u64, v: u64) -> u64 {
    (acc ^ round(0, v)).wrapping_mul(P1).wrapping_add(P4)
}

#[inline]
fn r64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}

#[inline]
fn r32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}

/// One-shot XXH64.
pub fn xxh64(data: &[u8], seed: u64) -> u64 {
    let mut p = data;
    let mut h = if data.len() >= 32 {
        let mut v1 = seed.wrapping_add(P1).wrapping_add(P2);
        let mut v2 = seed.wrapping_add(P2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(P1);
        while p.len() >= 32 {
            v1 = round(v1, r64(&p[0..]));
            v2 = round(v2, r64(&p[8..]));
            v3 = round(v3, r64(&p[16..]));
            v4 = round(v4, r64(&p[24..]));
            p = &p[32..];
        }
        let mut h = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
        h = merge(h, v1);
        h = merge(h, v2);
        h = merge(h, v3);
        merge(h, v4)
    } else {
        seed.wrapping_add(P5)
    };
    h = h.wrapping_add(data.len() as u64);
    while p.len() >= 8 {
        h ^= round(0, r64(p));
        h = h.rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
        p = &p[8..];
    }
    if p.len() >= 4 {
        h ^= u64::from(r32(p)).wrapping_mul(P1);
        h = h.rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
        p = &p[4..];
    }
    for &b in p {
        h ^= u64::from(b).wrapping_mul(P5);
        h = h.rotate_left(11).wrapping_mul(P1);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(P2);
    h ^= h >> 29;
    h = h.wrapping_mul(P3);
    h ^= h >> 32;
    h
}

#[cfg(test)]
mod tests {
    use super::xxh64;

    #[test]
    fn known_vectors() {
        assert_eq!(xxh64(b"", 0), 0xEF46_DB37_51D8_E999);
        assert_eq!(xxh64(b"a", 0), 0xD24E_C4F1_A98C_6E5B);
        assert_eq!(xxh64(b"abc", 0), 0x44BC_2CF5_AD77_0999);
    }
}
