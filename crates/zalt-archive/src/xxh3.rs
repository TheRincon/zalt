//! XXH3-128, the non-cryptographic `Hash_Algorithm` 1 (§7.3).
//!
//! Written from xxHash 0.8.2's reference (`xxhash.h`, the scalar paths), seed 0
//! and the default secret only — the only variant §7.3 names. It sits next to
//! the codec's own XXH64 rather than coming from a crate: the Rust ports all
//! carry `unsafe` for reads this does with `first_chunk`, and none is the
//! authors' own. The reference C is the oracle instead: `zlta-seekable-probe`
//! compiles it, and `tests/hash.rs` and the `archive_hash` fuzz target hold
//! this to it on every length class and every way of splitting the input.
//!
//! **Shape.** Inputs up to 240 bytes take one of four short paths (0–16,
//! 17–128, 129–240) that read the input from both ends. Longer ones run eight
//! 64-bit accumulators over 64-byte stripes, scrambled every 16 stripes, with
//! the final stripe always the input's last 64 bytes — which is why the
//! streaming state never consumes its buffer until more input arrives, and
//! keeps the 64 bytes before the buffer for an ending that falls short of a
//! stripe.
//!
//! **Digest bytes.** §7.2 says only "raw digest". This writes xxHash's
//! canonical form, `XXH128_canonicalFromHash`: the high 64 bits then the low,
//! each big-endian — what `xxhsum -H2` prints. `doc/ZALT-MAC.md` records the
//! gap as a spec finding.

const PRIME32_1: u64 = 0x9E37_79B1;
const PRIME32_2: u64 = 0x85EB_CA77;
const PRIME32_3: u64 = 0xC2B2_AE3D;
const PRIME64_1: u64 = 0x9E37_79B1_85EB_CA87;
const PRIME64_2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const PRIME64_3: u64 = 0x1656_67B1_9E37_79F9;
const PRIME64_4: u64 = 0x85EB_CA77_C2B2_AE63;
const PRIME64_5: u64 = 0x27D4_EB2F_1656_67C5;
const PRIME_MX1: u64 = 0x1656_6791_9E37_79F9;
const PRIME_MX2: u64 = 0x9FB2_1C65_1E98_DF25;

/// `XXH3_kSecret`, the default secret.
const SECRET: [u8; 192] = [
    0xb8, 0xfe, 0x6c, 0x39, 0x23, 0xa4, 0x4b, 0xbe, 0x7c, 0x01, 0x81, 0x2c, 0xf7, 0x21, 0xad, 0x1c,
    0xde, 0xd4, 0x6d, 0xe9, 0x83, 0x90, 0x97, 0xdb, 0x72, 0x40, 0xa4, 0xa4, 0xb7, 0xb3, 0x67, 0x1f,
    0xcb, 0x79, 0xe6, 0x4e, 0xcc, 0xc0, 0xe5, 0x78, 0x82, 0x5a, 0xd0, 0x7d, 0xcc, 0xff, 0x72, 0x21,
    0xb8, 0x08, 0x46, 0x74, 0xf7, 0x43, 0x24, 0x8e, 0xe0, 0x35, 0x90, 0xe6, 0x81, 0x3a, 0x26, 0x4c,
    0x3c, 0x28, 0x52, 0xbb, 0x91, 0xc3, 0x00, 0xcb, 0x88, 0xd0, 0x65, 0x8b, 0x1b, 0x53, 0x2e, 0xa3,
    0x71, 0x64, 0x48, 0x97, 0xa2, 0x0d, 0xf9, 0x4e, 0x38, 0x19, 0xef, 0x46, 0xa9, 0xde, 0xac, 0xd8,
    0xa8, 0xfa, 0x76, 0x3f, 0xe3, 0x9c, 0x34, 0x3f, 0xf9, 0xdc, 0xbb, 0xc7, 0xc7, 0x0b, 0x4f, 0x1d,
    0x8a, 0x51, 0xe0, 0x4b, 0xcd, 0xb4, 0x59, 0x31, 0xc8, 0x9f, 0x7e, 0xc9, 0xd9, 0x78, 0x73, 0x64,
    0xea, 0xc5, 0xac, 0x83, 0x34, 0xd3, 0xeb, 0xc3, 0xc5, 0x81, 0xa0, 0xff, 0xfa, 0x13, 0x63, 0xeb,
    0x17, 0x0d, 0xdd, 0x51, 0xb7, 0xf0, 0xda, 0x49, 0xd3, 0x16, 0x55, 0x26, 0x29, 0xd4, 0x68, 0x9e,
    0x2b, 0x16, 0xbe, 0x58, 0x7d, 0x47, 0xa1, 0xfc, 0x8f, 0xf8, 0xb8, 0xd1, 0x7a, 0xd0, 0x31, 0xce,
    0x45, 0xcb, 0x3a, 0x8f, 0x95, 0x16, 0x04, 0x28, 0xaf, 0xd7, 0xfb, 0xca, 0xbb, 0x4b, 0x40, 0x7e,
];

/// Bytes per stripe, and per accumulator round.
const STRIPE: usize = 64;
/// Secret bytes each stripe advances by.
const SECRET_CONSUME_RATE: usize = 8;
/// Stripes between scrambles: `(192 − 64) / 8`.
const STRIPES_PER_BLOCK: usize = (SECRET.len() - STRIPE) / SECRET_CONSUME_RATE;
/// Where the scramble's key starts: the secret's last stripe.
const SECRET_LIMIT: usize = SECRET.len() - STRIPE;
/// Offset of the last stripe's key, deliberately unaligned.
const SECRET_LASTACC_START: usize = 7;
const SECRET_MERGEACCS_START: usize = 11;
/// Largest input the short paths take.
const MIDSIZE_MAX: usize = 240;
const MIDSIZE_STARTOFFSET: usize = 3;
const MIDSIZE_LASTOFFSET: usize = 17;
/// `XXH3_SECRET_SIZE_MIN`.
const SECRET_SIZE_MIN: usize = 136;
/// The streaming buffer: four stripes.
const BUFFER: usize = 256;

const INIT_ACC: [u64; 8] = [
    PRIME32_3, PRIME64_1, PRIME64_2, PRIME64_3, PRIME64_4, PRIME32_2, PRIME64_5, PRIME32_1,
];

fn read32(b: &[u8], at: usize) -> u64 {
    let w: [u8; 4] = b[at..at + 4].try_into().expect("four bytes");
    u64::from(u32::from_le_bytes(w))
}

fn read64(b: &[u8], at: usize) -> u64 {
    let w: [u8; 8] = b[at..at + 8].try_into().expect("eight bytes");
    u64::from_le_bytes(w)
}

/// The full 128-bit product, as `(low, high)`.
fn mul128(a: u64, b: u64) -> (u64, u64) {
    let p = u128::from(a) * u128::from(b);
    (p as u64, (p >> 64) as u64)
}

fn mul128_fold64(a: u64, b: u64) -> u64 {
    let (lo, hi) = mul128(a, b);
    lo ^ hi
}

fn xorshift(v: u64, shift: u32) -> u64 {
    v ^ (v >> shift)
}

fn avalanche(mut h: u64) -> u64 {
    h = xorshift(h, 37);
    h = h.wrapping_mul(PRIME_MX1);
    xorshift(h, 32)
}

fn xxh64_avalanche(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(PRIME64_2);
    h ^= h >> 29;
    h = h.wrapping_mul(PRIME64_3);
    h ^ (h >> 32)
}

fn mix16(input: &[u8], at: usize, secret_at: usize) -> u64 {
    mul128_fold64(
        read64(input, at) ^ read64(&SECRET, secret_at),
        read64(input, at + 8) ^ read64(&SECRET, secret_at + 8),
    )
}

/// `XXH128_mix32B`, seed 0: `acc` is `(low, high)`.
fn mix32(acc: (u64, u64), input: &[u8], a: usize, b: usize, secret_at: usize) -> (u64, u64) {
    let low = acc.0.wrapping_add(mix16(input, a, secret_at))
        ^ read64(input, b).wrapping_add(read64(input, b + 8));
    let high = acc.1.wrapping_add(mix16(input, b, secret_at + 16))
        ^ read64(input, a).wrapping_add(read64(input, a + 8));
    (low, high)
}

/// The shared ending of the 17–240 byte paths.
fn finish_mid(acc: (u64, u64), len: u64) -> u128 {
    let low = acc.0.wrapping_add(acc.1);
    let high = acc
        .0
        .wrapping_mul(PRIME64_1)
        .wrapping_add(acc.1.wrapping_mul(PRIME64_4))
        .wrapping_add(len.wrapping_mul(PRIME64_2));
    join(avalanche(low), 0u64.wrapping_sub(avalanche(high)))
}

fn join(low: u64, high: u64) -> u128 {
    (u128::from(high) << 64) | u128::from(low)
}

fn len_0to16(input: &[u8]) -> u128 {
    let len = input.len();
    if len > 8 {
        let bitflip_l = read64(&SECRET, 32) ^ read64(&SECRET, 40);
        let bitflip_h = read64(&SECRET, 48) ^ read64(&SECRET, 56);
        let input_lo = read64(input, 0);
        let input_hi = read64(input, len - 8);
        let (mut m_lo, mut m_hi) = mul128(input_lo ^ input_hi ^ bitflip_l, PRIME64_1);
        m_lo = m_lo.wrapping_add(((len - 1) as u64) << 54);
        let input_hi = input_hi ^ bitflip_h;
        // The 64-bit form; the reference's 32-bit form computes the same.
        m_hi = m_hi
            .wrapping_add(input_hi)
            .wrapping_add((input_hi & 0xFFFF_FFFF).wrapping_mul(PRIME32_2 - 1));
        m_lo ^= m_hi.swap_bytes();
        let (h_lo, h_hi) = mul128(m_lo, PRIME64_2);
        let h_hi = h_hi.wrapping_add(m_hi.wrapping_mul(PRIME64_2));
        return join(avalanche(h_lo), avalanche(h_hi));
    }
    if len >= 4 {
        let input_lo = read32(input, 0);
        let input_hi = read32(input, len - 4);
        let input_64 = input_lo.wrapping_add(input_hi << 32);
        let bitflip = read64(&SECRET, 16) ^ read64(&SECRET, 24);
        let keyed = input_64 ^ bitflip;
        let (mut lo, mut hi) = mul128(keyed, PRIME64_1.wrapping_add((len as u64) << 2));
        hi = hi.wrapping_add(lo << 1);
        lo ^= hi >> 3;
        lo = xorshift(lo, 35);
        lo = lo.wrapping_mul(PRIME_MX2);
        lo = xorshift(lo, 28);
        return join(lo, avalanche(hi));
    }
    if len > 0 {
        let c1 = u32::from(input[0]);
        let c2 = u32::from(input[len >> 1]);
        let c3 = u32::from(input[len - 1]);
        let combined_l = (c1 << 16) | (c2 << 24) | c3 | ((len as u32) << 8);
        let combined_h = combined_l.swap_bytes().rotate_left(13);
        let bitflip_l = read32(&SECRET, 0) ^ read32(&SECRET, 4);
        let bitflip_h = read32(&SECRET, 8) ^ read32(&SECRET, 12);
        return join(
            xxh64_avalanche(u64::from(combined_l) ^ bitflip_l),
            xxh64_avalanche(u64::from(combined_h) ^ bitflip_h),
        );
    }
    let bitflip_l = read64(&SECRET, 64) ^ read64(&SECRET, 72);
    let bitflip_h = read64(&SECRET, 80) ^ read64(&SECRET, 88);
    join(xxh64_avalanche(bitflip_l), xxh64_avalanche(bitflip_h))
}

fn len_17to128(input: &[u8]) -> u128 {
    let len = input.len();
    let mut acc = ((len as u64).wrapping_mul(PRIME64_1), 0u64);
    if len > 32 {
        if len > 64 {
            if len > 96 {
                acc = mix32(acc, input, 48, len - 64, 96);
            }
            acc = mix32(acc, input, 32, len - 48, 64);
        }
        acc = mix32(acc, input, 16, len - 32, 32);
    }
    acc = mix32(acc, input, 0, len - 16, 0);
    finish_mid(acc, len as u64)
}

fn len_129to240(input: &[u8]) -> u128 {
    let len = input.len();
    let mut acc = ((len as u64).wrapping_mul(PRIME64_1), 0u64);
    let mut i = 32;
    while i < 160 {
        acc = mix32(acc, input, i - 32, i - 16, i - 32);
        i += 32;
    }
    acc = (avalanche(acc.0), avalanche(acc.1));
    let mut i = 160;
    while i <= len {
        acc = mix32(acc, input, i - 32, i - 16, MIDSIZE_STARTOFFSET + i - 160);
        i += 32;
    }
    // The last 32 bytes, with the halves the other way round. Seed 0, so the
    // reference's `0 - seed` is 0.
    acc = mix32(
        acc,
        input,
        len - 16,
        len - 32,
        SECRET_SIZE_MIN - MIDSIZE_LASTOFFSET - 16,
    );
    finish_mid(acc, len as u64)
}

/// One stripe into the accumulators, keyed from `SECRET[key..]`.
fn accumulate_512(acc: &mut [u64; 8], stripe: &[u8], key: usize) {
    for lane in 0..8 {
        let data = read64(stripe, lane * 8);
        let keyed = data ^ read64(&SECRET, key + lane * 8);
        acc[lane ^ 1] = acc[lane ^ 1].wrapping_add(data);
        acc[lane] = (keyed & 0xFFFF_FFFF)
            .wrapping_mul(keyed >> 32)
            .wrapping_add(acc[lane]);
    }
}

/// `n` consecutive stripes from `input`, the first keyed at stripe `first`
/// of the block.
fn accumulate(acc: &mut [u64; 8], input: &[u8], n: usize, first: usize) {
    for s in 0..n {
        accumulate_512(
            acc,
            &input[s * STRIPE..(s + 1) * STRIPE],
            (first + s) * SECRET_CONSUME_RATE,
        );
    }
}

fn scramble(acc: &mut [u64; 8]) {
    for (lane, a) in acc.iter_mut().enumerate() {
        let key = read64(&SECRET, SECRET_LIMIT + lane * 8);
        *a = (xorshift(*a, 47) ^ key).wrapping_mul(PRIME32_1);
    }
}

fn merge(acc: &[u64; 8], secret_at: usize, start: u64) -> u64 {
    let mut result = start;
    for i in 0..4 {
        result = result.wrapping_add(mul128_fold64(
            acc[2 * i] ^ read64(&SECRET, secret_at + 16 * i),
            acc[2 * i + 1] ^ read64(&SECRET, secret_at + 16 * i + 8),
        ));
    }
    avalanche(result)
}

/// Both halves of a long hash from its finished accumulators.
fn finish_long(acc: &[u64; 8], len: u64) -> u128 {
    let low = merge(acc, SECRET_MERGEACCS_START, len.wrapping_mul(PRIME64_1));
    let high = merge(
        acc,
        SECRET.len() - 64 - SECRET_MERGEACCS_START,
        !len.wrapping_mul(PRIME64_2),
    );
    join(low, high)
}

/// XXH3-128 of `input`, in one call.
pub fn hash(input: &[u8]) -> u128 {
    let len = input.len();
    if len <= 16 {
        return len_0to16(input);
    }
    if len <= 128 {
        return len_17to128(input);
    }
    if len <= MIDSIZE_MAX {
        return len_129to240(input);
    }
    // Blocks of 16 stripes, then the stripes of the last partial block, then
    // the input's final 64 bytes, always — overlapping the stripes before
    // them when the length is not a multiple of 64.
    let mut acc = INIT_ACC;
    let block = STRIPE * STRIPES_PER_BLOCK;
    let blocks = (len - 1) / block;
    for b in 0..blocks {
        accumulate(&mut acc, &input[b * block..], STRIPES_PER_BLOCK, 0);
        scramble(&mut acc);
    }
    let stripes = ((len - 1) - block * blocks) / STRIPE;
    accumulate(&mut acc, &input[blocks * block..], stripes, 0);
    accumulate_512(
        &mut acc,
        &input[len - STRIPE..],
        SECRET_LIMIT - SECRET_LASTACC_START,
    );
    finish_long(&acc, len as u64)
}

/// The canonical 16-byte form: high half, then low, each big-endian.
pub fn canonical(h: u128) -> [u8; 16] {
    h.to_be_bytes()
}

/// XXH3-128 over input that arrives in pieces — `XXH3_state_t`, seed 0.
///
/// Any split of the input gives the same hash as [`hash`] on the whole.
#[derive(Clone)]
pub struct Xxh3 {
    acc: [u64; 8],
    /// Input not yet accumulated. Never emptied by an update: the final
    /// stripe is the input's last 64 bytes, and only the digest knows which
    /// those are.
    buffer: [u8; BUFFER],
    buffered: usize,
    /// Stripes accumulated into the current block, 0 to 15.
    stripes_so_far: usize,
    total: u64,
}

impl Default for Xxh3 {
    fn default() -> Self {
        Self::new()
    }
}

impl Xxh3 {
    pub fn new() -> Self {
        Self {
            acc: INIT_ACC,
            buffer: [0; BUFFER],
            buffered: 0,
            stripes_so_far: 0,
            total: 0,
        }
    }

    /// `XXH3_consumeStripes`: `n` whole stripes from `input`, scrambling at
    /// each block boundary. Returns the bytes consumed.
    fn consume(
        acc: &mut [u64; 8],
        stripes_so_far: &mut usize,
        input: &[u8],
        mut n: usize,
    ) -> usize {
        let mut at = 0;
        while n > 0 {
            let room = STRIPES_PER_BLOCK - *stripes_so_far;
            let take = n.min(room);
            accumulate(acc, &input[at..], take, *stripes_so_far);
            at += take * STRIPE;
            n -= take;
            *stripes_so_far += take;
            if *stripes_so_far == STRIPES_PER_BLOCK {
                scramble(acc);
                *stripes_so_far = 0;
            }
        }
        at
    }

    pub fn update(&mut self, mut input: &[u8]) {
        self.total += input.len() as u64;
        // Fits: buffer it. Strictly more than fits is what lets the buffer be
        // consumed below, knowing it does not hold the input's last stripe.
        if input.len() <= BUFFER - self.buffered {
            self.buffer[self.buffered..self.buffered + input.len()].copy_from_slice(input);
            self.buffered += input.len();
            return;
        }
        if self.buffered > 0 {
            let load = BUFFER - self.buffered;
            self.buffer[self.buffered..].copy_from_slice(&input[..load]);
            input = &input[load..];
            let buffer = self.buffer;
            Self::consume(
                &mut self.acc,
                &mut self.stripes_so_far,
                &buffer,
                BUFFER / STRIPE,
            );
            self.buffered = 0;
        }
        if input.len() > BUFFER {
            // Whole stripes straight from the input, leaving at least one
            // byte, and keeping the stripe just consumed in the buffer's last
            // 64 bytes for a digest that ends short of a stripe.
            let stripes = (input.len() - 1) / STRIPE;
            let used = Self::consume(&mut self.acc, &mut self.stripes_so_far, input, stripes);
            self.buffer[BUFFER - STRIPE..].copy_from_slice(&input[used - STRIPE..used]);
            input = &input[used..];
        }
        self.buffer[..input.len()].copy_from_slice(input);
        self.buffered = input.len();
    }

    pub fn digest(&self) -> u128 {
        if self.total <= MIDSIZE_MAX as u64 {
            return hash(&self.buffer[..self.buffered]);
        }
        let mut acc = self.acc;
        let mut stripes_so_far = self.stripes_so_far;
        let mut last = [0u8; STRIPE];
        if self.buffered >= STRIPE {
            let stripes = (self.buffered - 1) / STRIPE;
            Self::consume(&mut acc, &mut stripes_so_far, &self.buffer, stripes);
            last.copy_from_slice(&self.buffer[self.buffered - STRIPE..self.buffered]);
        } else {
            // The last stripe reaches back before the buffer's content, into
            // the bytes kept at its end.
            let catch_up = STRIPE - self.buffered;
            last[..catch_up].copy_from_slice(&self.buffer[BUFFER - catch_up..]);
            last[catch_up..].copy_from_slice(&self.buffer[..self.buffered]);
        }
        accumulate_512(&mut acc, &last, SECRET_LIMIT - SECRET_LASTACC_START);
        finish_long(&acc, self.total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// xxHash's published value for empty input (`xxhsum -H2` of an empty
    /// file), independent of both this code and the reference it is tested
    /// against.
    #[test]
    fn empty_input_has_the_published_hash() {
        let hex: String = canonical(hash(b""))
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(hex, "99aa06d3014798d86001c324468d497f");
    }

    /// Splitting must never change the hash — the property streaming exists
    /// for, checked across every length class and the buffer's edges. Against
    /// the reference implementation itself in `tests/hash.rs`.
    #[test]
    fn any_split_gives_the_one_shot_hash() {
        let data: Vec<u8> = (0..3000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        for len in [
            0, 1, 3, 4, 8, 9, 16, 17, 128, 129, 240, 241, 255, 256, 257, 320, 1024, 1025, 2049,
            3000,
        ] {
            let whole = hash(&data[..len]);
            for split in [1usize, 7, 63, 64, 65, 255, 256, 257, 1000] {
                let mut s = Xxh3::new();
                for piece in data[..len].chunks(split) {
                    s.update(piece);
                }
                assert_eq!(s.digest(), whole, "len {len}, pieces of {split}");
            }
        }
    }
}
