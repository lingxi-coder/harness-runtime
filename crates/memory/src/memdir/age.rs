//! Fixed-point u64 age weight (no f64).

#![deny(clippy::float_arithmetic)]

use crate::{MEMORY_AGE_PENALTY_DAYS, MEMORY_MIN_AGE_WEIGHT_BPS};

/// Age weight in basis points.
///
/// Formula: `max(MEMORY_MIN_AGE_WEIGHT_BPS, 10_000 / (1 + age_blocks))`
/// where `age_blocks = age_days / MEMORY_AGE_PENALTY_DAYS` (integer
/// division). A brand-new entry (`age_days` = 0 → `age_blocks` = 0) yields
/// `10_000 / 1 = 10_000` (no penalty). After 30 days the weight halves
/// to `10_000 / 2 = 5_000`. After 9 blocks the weight bottoms out at
/// the floor `1_000`.
#[must_use]
pub const fn age_weight_bps(age_days: u64) -> u32 {
    let age_blocks = age_days / MEMORY_AGE_PENALTY_DAYS;
    // u64 arithmetic throughout (the module denies float arithmetic). The
    // quotient is bounded by `10_000 / 1 = 10_000`, so the narrowing cast
    // to u32 is exact — but we guard it explicitly anyway.
    let raw = 10_000_u64 / (1 + age_blocks);
    let raw_u32: u32 = if raw > u32::MAX as u64 {
        u32::MAX
    } else {
        // Safe: numerator 10_000 means raw <= 10_000, which fits in u32.
        #[allow(clippy::cast_possible_truncation)]
        let r = raw as u32;
        r
    };
    if raw_u32 < MEMORY_MIN_AGE_WEIGHT_BPS {
        MEMORY_MIN_AGE_WEIGHT_BPS
    } else {
        raw_u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_new_entry_full_weight() {
        assert_eq!(age_weight_bps(0), 10_000);
        assert_eq!(age_weight_bps(29), 10_000); // still in block 0
    }

    #[test]
    fn block_one_halves_weight() {
        // 30 days → 1 block → 10_000 / 2 = 5_000
        assert_eq!(age_weight_bps(30), 5_000);
        assert_eq!(age_weight_bps(59), 5_000);
    }

    #[test]
    fn block_two_third_weight() {
        // 60 days → 2 blocks → 10_000 / 3 = 3_333
        assert_eq!(age_weight_bps(60), 3_333);
    }

    #[test]
    fn very_old_clamped_to_min() {
        // 10_000 / 10 = 1_000 (the floor). 10_000 / 11 = 909 → clamped to 1_000.
        assert_eq!(age_weight_bps(270), 1_000); // 9 blocks → 10_000/10 = 1_000
        assert_eq!(age_weight_bps(300), 1_000); // 10 blocks → 10_000/11 = 909 → 1_000
        assert_eq!(age_weight_bps(364), 1_000); // still under hard drop, still 1_000
    }

    #[test]
    fn deterministic_no_float() {
        // Same input, same output, always.
        for d in [0u64, 1, 30, 31, 60, 200, 364] {
            assert_eq!(age_weight_bps(d), age_weight_bps(d));
        }
    }
}
