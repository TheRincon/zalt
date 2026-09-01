//! XXH64, used for the zstd frame content checksum.
//!
//! The frame checksum is the **low 32 bits** of the XXH64 of the decoded
//! content. Streaming decode means this must be computable incrementally over
//! arbitrarily-chunked input, so the state machine here is the point, not the
//! one-shot convenience function.

const PRIME1: u64 = 0x9E37_79B1_85EB_CA87;
const PRIME2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const PRIME3: u64 = 0x1656_67B1_9E37_79F9;
const PRIME4: u64 = 0x85EB_CA77_C2B2_AE63;
const PRIME5: u64 = 0x27D4_EB2F_1656_67C5;

/// Incremental XXH64.
#[derive(Debug, Clone)]
pub struct Xxh64 {
    acc: [u64; 4],
    buf: [u8; 32],
    buf_len: usize,
    total_len: u64,
    seed: u64,
}

impl Default for Xxh64 {
    fn default() -> Self {
        Self::new(0)
    }
}

impl Xxh64 {
    /// Create a new hasher.
    pub fn new(seed: u64) -> Self {
        Self {
            acc: [
                seed.wrapping_add(PRIME1).wrapping_add(PRIME2),
                seed.wrapping_add(PRIME2),
                seed,
                seed.wrapping_sub(PRIME1),
            ],
            buf: [0; 32],
            buf_len: 0,
            total_len: 0,
            seed,
        }
    }

    #[inline]
    fn round(mut acc: u64, input: u64) -> u64 {
        acc = acc.wrapping_add(input.wrapping_mul(PRIME2));
        acc = acc.rotate_left(31);
        acc.wrapping_mul(PRIME1)
    }

    #[inline]
    fn merge_round(mut acc: u64, val: u64) -> u64 {
        acc ^= Self::round(0, val);
        acc = acc.wrapping_mul(PRIME1);
        acc.wrapping_add(PRIME4)
    }

    /// Feed more data. Safe to call with any chunking.
    pub fn update(&mut self, mut data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);

        // Top up a partial stripe first.
        if self.buf_len > 0 {
            let take = core::cmp::min(32 - self.buf_len, data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len < 32 {
                return;
            }
            let stripe = self.buf;
            self.consume_stripe(&stripe);
            self.buf_len = 0;
        }

        // Whole stripes.
        //
        // Borrowed, not copied. The obvious formulation reads each stripe into
        // a local `[u8; 32]` first, which doubles the checksum's memory traffic
        // — every byte of output moved once more for no reason. `chunks_exact`
        // already guarantees the length, so the conversion is infallible; it is
        // written as a match rather than an `unwrap` because nothing reachable
        // from parsing may panic, and a check the optimiser removes costs
        // nothing to keep.
        // The four accumulators are hoisted into locals for the whole run and
        // written back once. Reached through `&mut self` they are a field load
        // and store per round; the four chains are independent of each other
        // and *only* of each other, so keeping them in registers across stripes
        // is what lets the four dependent multiply chains overlap at all.
        let mut acc = self.acc;
        let mut chunks = data.chunks_exact(32);
        for stripe in &mut chunks {
            if let Ok(s) = <&[u8; 32]>::try_from(stripe) {
                for (a, lane) in acc.iter_mut().zip(s.chunks_exact(8)) {
                    if let Ok(bytes) = <&[u8; 8]>::try_from(lane) {
                        *a = Self::round(*a, u64::from_le_bytes(*bytes));
                    }
                }
            }
        }
        self.acc = acc;

        // Stash the remainder.
        let rem = chunks.remainder();
        self.buf[..rem.len()].copy_from_slice(rem);
        self.buf_len = rem.len();
    }

    #[inline]
    fn consume_stripe(&mut self, stripe: &[u8; 32]) {
        for i in 0..4 {
            let lane = u64::from_le_bytes(stripe[i * 8..i * 8 + 8].try_into().unwrap());
            self.acc[i] = Self::round(self.acc[i], lane);
        }
    }

    /// Finish and produce the 64-bit digest.
    pub fn digest(&self) -> u64 {
        let mut acc = if self.total_len >= 32 {
            let mut a = self.acc[0]
                .rotate_left(1)
                .wrapping_add(self.acc[1].rotate_left(7))
                .wrapping_add(self.acc[2].rotate_left(12))
                .wrapping_add(self.acc[3].rotate_left(18));
            a = Self::merge_round(a, self.acc[0]);
            a = Self::merge_round(a, self.acc[1]);
            a = Self::merge_round(a, self.acc[2]);
            a = Self::merge_round(a, self.acc[3]);
            a
        } else {
            self.seed.wrapping_add(PRIME5)
        };

        acc = acc.wrapping_add(self.total_len);

        let mut tail = &self.buf[..self.buf_len];

        while tail.len() >= 8 {
            let lane = u64::from_le_bytes(tail[..8].try_into().unwrap());
            acc ^= Self::round(0, lane);
            acc = acc
                .rotate_left(27)
                .wrapping_mul(PRIME1)
                .wrapping_add(PRIME4);
            tail = &tail[8..];
        }

        if tail.len() >= 4 {
            let lane = u32::from_le_bytes(tail[..4].try_into().unwrap());
            acc ^= u64::from(lane).wrapping_mul(PRIME1);
            acc = acc
                .rotate_left(23)
                .wrapping_mul(PRIME2)
                .wrapping_add(PRIME3);
            tail = &tail[4..];
        }

        for &b in tail {
            acc ^= u64::from(b).wrapping_mul(PRIME5);
            acc = acc.rotate_left(11).wrapping_mul(PRIME1);
        }

        acc ^= acc >> 33;
        acc = acc.wrapping_mul(PRIME2);
        acc ^= acc >> 29;
        acc = acc.wrapping_mul(PRIME3);
        acc ^= acc >> 32;
        acc
    }

    /// The 32-bit value zstd stores in a frame's content checksum field.
    pub fn frame_checksum(&self) -> u32 {
        self.digest() as u32
    }
}

/// One-shot convenience.
pub fn xxh64(data: &[u8], seed: u64) -> u64 {
    let mut h = Xxh64::new(seed);
    h.update(data);
    h.digest()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn empty_input_reference_vector() {
        assert_eq!(xxh64(b"", 0), 0xEF46_DB37_51D8_E999);
    }

    /// Chunking must not change the digest. This is the property that actually
    /// matters for streaming decode, and the one most likely to regress.
    #[test]
    fn streaming_matches_one_shot_at_every_split() {
        // Quadratic in `len`: every split point re-hashes the whole input. 1000
        // bytes is a million update calls, which Miri cannot interpret in
        // reasonable time. The property is the same at 120 bytes — it still
        // crosses stripe boundaries in both halves, which is where a split bug
        // would live.
        let len = if cfg!(miri) { 120 } else { 1000 };
        let data: Vec<u8> = (0..=255u8).cycle().take(len).collect();
        let expected = xxh64(&data, 0);

        for split in 0..data.len() {
            let mut h = Xxh64::new(0);
            h.update(&data[..split]);
            h.update(&data[split..]);
            assert_eq!(h.digest(), expected, "split at {split}");
        }
    }

    #[test]
    fn byte_at_a_time_matches_one_shot() {
        let data: Vec<u8> = (0..200u8).collect();
        let expected = xxh64(&data, 0);
        let mut h = Xxh64::new(0);
        for b in &data {
            h.update(core::slice::from_ref(b));
        }
        assert_eq!(h.digest(), expected);
    }

    #[test]
    fn seed_changes_digest() {
        assert_ne!(xxh64(b"zalt", 0), xxh64(b"zalt", 1));
    }

    #[test]
    fn lengths_around_stripe_boundaries() {
        // 31/32/33 and 63/64/65 exercise the stripe and tail paths.
        for len in [0, 1, 3, 4, 7, 8, 31, 32, 33, 63, 64, 65, 127, 128] {
            let data: Vec<u8> = (0..len as u8).collect();
            let mut h = Xxh64::new(0);
            h.update(&data);
            assert_eq!(h.digest(), xxh64(&data, 0), "len {len}");
        }
    }
}
