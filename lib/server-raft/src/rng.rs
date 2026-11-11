//! A small seeded random number generator for tests and the simulator.
//!
//! SplitMix64 (Steele, Lea, and Flood, "Fast splittable pseudorandom
//! number generators", OOPSLA 2014): one `u64` of state, and the same seed
//! always gives the same stream on every platform. It is not for
//! cryptography.

/// A deterministic random stream.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A value in `0..n`; `n` must be positive.
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "Rng::below(0)");
        // Multiply-shift: unbiased enough for test choices.
        ((u128::from(self.next_u64()) * u128::from(n)) >> 64) as u64
    }

    /// A value in `lo..=hi`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stream_is_fixed_by_the_seed() {
        let draws = |seed| {
            let mut rng = Rng::new(seed);
            [rng.next_u64(), rng.next_u64(), rng.next_u64()]
        };
        assert_eq!(draws(7), draws(7));
        assert_ne!(draws(7), draws(8));
        // The SplitMix64 reference value for seed 0.
        assert_eq!(Rng::new(0).next_u64(), 0xe220_a839_7b1d_cdaf);
    }

    #[test]
    fn values_stay_in_range() {
        let mut rng = Rng::new(3);
        for _ in 0..10_000 {
            assert!(rng.below(7) < 7);
            let v = rng.range(3, 5);
            assert!((3..=5).contains(&v));
        }
    }
}
