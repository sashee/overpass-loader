//! A small deterministic random number generator (SplitMix64), so generated
//! inputs depend only on the seed and the generator's code.

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let z = self.0;
        let z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        let z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`; `n` must be positive.
    pub fn below(&mut self, n: u64) -> u64 {
        // Rejection sampling keeps the distribution exactly uniform.
        let zone = u64::MAX - (u64::MAX % n);
        loop {
            let x = self.next_u64();
            if x < zone {
                return x % n;
            }
        }
    }

    /// Uniform in `lo..=hi`.
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo) as u64 + 1) as i64
    }

    /// True with probability `percent` / 100.
    pub fn percent(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_sequence() {
        let (mut a, mut b) = (Rng::new(7), Rng::new(7));
        let xs: Vec<u64> = (0..5).map(|_| a.next_u64()).collect();
        let ys: Vec<u64> = (0..5).map(|_| b.next_u64()).collect();
        assert_eq!(xs, ys);
        assert_ne!(Rng::new(8).next_u64(), xs[0]);
    }

    #[test]
    fn known_first_value() {
        // Reference value of SplitMix64 for seed 0; pins the sequence so
        // generated inputs cannot change silently.
        assert_eq!(Rng::new(0).next_u64(), 0xe220_a839_7b1d_cdaf);
    }

    #[test]
    fn ranges_are_inclusive_and_bounded() {
        let mut rng = Rng::new(1);
        let values: Vec<i64> = (0..1000).map(|_| rng.range(-2, 2)).collect();
        assert!(values.iter().all(|v| (-2..=2).contains(v)));
        assert!((-2..=2).all(|v| values.contains(&v)));
    }
}
