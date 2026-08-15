//! The one and only source of randomness in Current (D-6, I-2).
//!
//! Everything random in this repository comes from here, and everything here comes from a seed.
//! No engine code, no oracle code, and no operator ever calls this — the scenario generator does,
//! and nothing else. That is what makes a failing scenario reproducible from its seed and a
//! passing suite meaningful.
//!
//! ## Why ChaCha8 and not the system RNG or a fashionable fast one
//!
//! [`rand_chacha::ChaCha8Rng`] is *value-stable*: a given seed produces the same stream forever,
//! across versions, platforms, and pointer widths. A generator whose output drifts with a
//! dependency bump would silently retire the corpus of seeds that have ever been run — including
//! the seed attached to a bug report. Speed is irrelevant here; stability is the whole
//! requirement.

use rand_chacha::ChaCha8Rng;
// Imported anonymously: the trait supplies `next_u64`, and its name would collide with the
// `Rng` defined below.
use rand_core::{Rng as _, SeedableRng};

/// A seeded, reproducible source of random choices.
#[derive(Debug, Clone)]
pub struct Rng {
    inner: ChaCha8Rng,
}

impl Rng {
    #[must_use]
    pub fn from_seed(seed: u64) -> Rng {
        Rng {
            inner: ChaCha8Rng::seed_from_u64(seed),
        }
    }

    fn next_u64(&mut self) -> u64 {
        self.inner.next_u64()
    }

    /// A uniform value in `0..n`. Returns 0 when `n` is 0, so that a caller who asks for a
    /// choice among nothing gets a defined answer rather than a panic.
    ///
    /// Uses rejection sampling rather than a plain modulo: `next_u64() % n` is biased toward
    /// small values unless `n` divides 2⁶⁴, and a biased generator quietly under-tests whatever
    /// lives at the top of the range.
    pub fn below(&mut self, n: u64) -> u64 {
        if n <= 1 {
            return 0;
        }
        // 2^64 mod n, computed without needing to represent 2^64.
        let threshold = n.wrapping_neg() % n;
        loop {
            let x = self.next_u64();
            if x >= threshold {
                return x % n;
            }
        }
    }

    /// A uniform value in `lo..=hi`. `lo` is returned if the range is empty or inverted.
    pub fn between(&mut self, lo: i64, hi: i64) -> i64 {
        if hi <= lo {
            return lo;
        }
        let span = hi.wrapping_sub(lo) as u64 + 1;
        lo.wrapping_add(self.below(span) as i64)
    }

    /// True with probability `numerator / denominator`.
    pub fn chance(&mut self, numerator: u64, denominator: u64) -> bool {
        if denominator == 0 {
            return false;
        }
        self.below(denominator) < numerator
    }

    /// Pick one item. Returns `None` only for an empty slice.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() {
            return None;
        }
        let index = self.below(items.len() as u64) as usize;
        items.get(index)
    }

    /// Pick an index into a collection of `len` items. Returns `None` only for `len == 0`.
    pub fn pick_index(&mut self, len: usize) -> Option<usize> {
        if len == 0 {
            return None;
        }
        Some(self.below(len as u64) as usize)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn the_same_seed_gives_the_same_stream() {
        let mut a = Rng::from_seed(12345);
        let mut b = Rng::from_seed(12345);
        for _ in 0..1000 {
            assert_eq!(a.below(1_000_000), b.below(1_000_000));
        }
    }

    #[test]
    fn different_seeds_give_different_streams() {
        let mut a = Rng::from_seed(1);
        let mut b = Rng::from_seed(2);
        let xs: Vec<u64> = (0..32).map(|_| a.below(u64::MAX)).collect();
        let ys: Vec<u64> = (0..32).map(|_| b.below(u64::MAX)).collect();
        assert_ne!(xs, ys);
    }

    #[test]
    fn below_stays_in_range_and_handles_degenerate_bounds() {
        let mut rng = Rng::from_seed(7);
        for n in [1_u64, 2, 3, 7, 100] {
            for _ in 0..200 {
                assert!(rng.below(n) < n);
            }
        }
        assert_eq!(rng.below(0), 0);
        assert_eq!(rng.below(1), 0);
    }

    #[test]
    fn between_is_inclusive_at_both_ends_and_covers_the_range() {
        let mut rng = Rng::from_seed(99);
        let mut seen_lo = false;
        let mut seen_hi = false;
        for _ in 0..500 {
            let v = rng.between(-2, 2);
            assert!((-2..=2).contains(&v));
            seen_lo |= v == -2;
            seen_hi |= v == 2;
        }
        assert!(seen_lo && seen_hi, "both endpoints must be reachable");
        assert_eq!(rng.between(5, 5), 5);
        assert_eq!(rng.between(5, 1), 5);
    }

    #[test]
    fn pick_returns_none_only_for_an_empty_slice() {
        let mut rng = Rng::from_seed(3);
        let empty: [u8; 0] = [];
        assert!(rng.pick(&empty).is_none());
        assert!(rng.pick(&[1, 2, 3]).is_some());
        assert!(rng.pick_index(0).is_none());
        assert_eq!(rng.pick_index(1), Some(0));
    }

    #[test]
    fn the_stream_is_value_stable_for_a_known_seed() {
        // A canary: if a dependency bump changes these numbers, every seed ever recorded in a
        // bug report has silently stopped meaning what it meant, and this test says so.
        let mut rng = Rng::from_seed(0);
        let got: Vec<u64> = (0..4).map(|_| rng.below(1_000_000)).collect();
        assert_eq!(got, vec![68652, 413623, 187878, 354556]);
    }
}
