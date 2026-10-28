//! Small deterministic hashing and random number helpers.
//!
//! Generated datasets must be reproducible from the same seed files, so UIDs and
//! demographics are derived by hashing rather than drawn from an OS-seeded generator.

/// 64-bit FNV-1a over `parts`, with `seed` mixed into the offset basis.
pub fn hash(seed: u64, parts: &[&[u8]]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325 ^ seed.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    for part in parts {
        for byte in *part {
            h ^= u64::from(*byte);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        // Separator, so that ("ab", "c") and ("a", "bc") differ.
        h ^= 0xff;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    mix(h)
}

/// SplitMix64 finalizer, spreads FNV output over all bits.
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Fast non-cryptographic generator for picking request targets.
#[derive(Debug, Clone)]
pub struct SplitMix(u64);

impl SplitMix {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        mix(self.0)
    }

    /// Uniform index in `0..len`; `len` must be non-zero.
    pub fn index(&mut self, len: usize) -> usize {
        (self.next_u64() % len as u64) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_deterministic_and_part_aware() {
        assert_eq!(hash(1, &[b"abc"]), hash(1, &[b"abc"]));
        assert_ne!(hash(1, &[b"abc"]), hash(2, &[b"abc"]));
        assert_ne!(hash(0, &[b"ab", b"c"]), hash(0, &[b"a", b"bc"]));
    }
}
