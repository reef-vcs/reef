//! Random draws built only from the raw `ChaCha8` stream and IEEE 754 basic
//! arithmetic (`+ - * /`, `sqrt`, `round`), so every platform gets the same
//! values bit for bit. libm functions such as `exp`, `ln`, and `cos` are not
//! correctly rounded and differ between macOS and glibc, so they are never
//! used here; [`exp`] is implemented with a polynomial instead. Nothing here
//! goes through `rand`'s distributions either, whose output may change
//! between `rand` releases.

use rand::Rng;

/// The random stream the generator draws from.
pub(crate) type SynthRng = rand_chacha::ChaCha8Rng;

/// A uniform integer in `0..n`. `n` must be non-zero.
///
/// Lemire's widening-multiply method with rejection, so the result is
/// unbiased and consumes a platform-independent number of draws.
pub(crate) fn below(rng: &mut SynthRng, n: u64) -> u64 {
    debug_assert!(n > 0, "below(0)");
    let threshold = n.wrapping_neg() % n;
    loop {
        let wide = u128::from(rng.next_u64()) * u128::from(n);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "taking the low 64 bits is the method"
        )]
        let low = wide as u64;
        if low >= threshold {
            return (wide >> 64) as u64;
        }
    }
}

/// A uniform index into a collection of length `len`, which must be non-zero.
pub(crate) fn index(rng: &mut SynthRng, len: usize) -> usize {
    // usize is at most 64 bits on every supported target, so both casts are lossless.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the result is below len, which fits"
    )]
    let i = below(rng, len as u64) as usize;
    i
}

/// A uniform `f64` in `[0, 1)` with 53 bits of precision.
pub(crate) fn unit(rng: &mut SynthRng) -> f64 {
    #[allow(clippy::cast_precision_loss, reason = "the value has 53 bits")]
    let mantissa = (rng.next_u64() >> 11) as f64;
    mantissa * (1.0 / 9_007_199_254_740_992.0)
}

/// An approximately standard normal draw: the sum of twelve uniforms minus
/// six (Irwin–Hall). Mean 0, variance 1, bounded to `[-6, 6]`, and free of
/// transcendental functions.
pub(crate) fn normal(rng: &mut SynthRng) -> f64 {
    let mut sum = 0.0;
    for _ in 0..12 {
        sum += unit(rng);
    }
    sum - 6.0
}

/// A Poisson-distributed count with mean `mean`, which must be finite and
/// non-negative. Knuth's multiplication method for small means, a rounded
/// normal approximation above 30 where Knuth's method gets slow.
pub(crate) fn poisson(rng: &mut SynthRng, mean: f64) -> u64 {
    if mean <= 0.0 {
        return 0;
    }
    if mean > 30.0 {
        return to_u64(mean + mean.sqrt() * normal(rng));
    }
    let limit = exp(-mean);
    let mut count = 0;
    let mut product = unit(rng);
    while product > limit {
        count += 1;
        product *= unit(rng);
    }
    count
}

/// A log-normal draw with the given median and shape `sigma`, rounded to an
/// integer and clamped to `1..=max`.
pub(crate) fn log_normal(rng: &mut SynthRng, median: u64, sigma: f64, max: u64) -> u64 {
    #[allow(clippy::cast_precision_loss, reason = "sizes are far below 2^53")]
    let median = median as f64;
    to_u64(median * exp(sigma * normal(rng))).clamp(1, max)
}

/// `f64` to `u64`, rounding to nearest and saturating at both ends.
pub(crate) fn to_u64(x: f64) -> u64 {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "float-to-int `as` saturates, which is the intent"
    )]
    let n = x.round() as u64;
    n
}

/// `e^x` for `|x| <= 700`, correct to a few ulp and identical on every
/// IEEE 754 platform: `x = k ln 2 + r` with `|r| <= ln 2 / 2`, a degree-13
/// Taylor polynomial for `e^r`, and `2^k` built from its exponent bits.
pub(crate) fn exp(x: f64) -> f64 {
    const LN_2: f64 = std::f64::consts::LN_2;
    let x = x.clamp(-700.0, 700.0);
    let k = (x / LN_2).round();
    let r = x - k * LN_2;
    let mut term = 1.0;
    let mut sum = 1.0;
    for n in 1..=13 {
        term = term * r / f64::from(n);
        sum += term;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "k is in -1010..=1010, so k + 1023 is a valid biased exponent"
    )]
    let biased = (k as i64 + 1023) as u64;
    sum * f64::from_bits(biased << 52)
}

/// Picks `count` distinct indices from `0..len` in random order with a
/// partial Fisher–Yates shuffle. `count` must not exceed `len`.
pub(crate) fn sample(rng: &mut SynthRng, len: usize, count: usize) -> Vec<usize> {
    debug_assert!(count <= len, "sample({len}, {count})");
    let mut pool: Vec<usize> = (0..len).collect();
    for i in 0..count {
        let j = i + index(rng, len - i);
        pool.swap(i, j);
    }
    pool.truncate(count);
    pool
}

#[cfg(test)]
mod tests {
    use rand::SeedableRng;

    use super::*;

    #[test]
    fn exp_matches_std_closely() {
        for i in -200..=200 {
            let x = f64::from(i) * 0.05;
            let ours = exp(x);
            let std = x.exp();
            assert!(
                ((ours - std) / std).abs() < 1e-14,
                "exp({x}): {ours} vs {std}"
            );
        }
    }

    #[test]
    fn below_stays_in_range() {
        let mut rng = SynthRng::seed_from_u64(0);
        for n in [1, 2, 3, 7, 1000, u64::MAX] {
            for _ in 0..100 {
                assert!(below(&mut rng, n) < n);
            }
        }
    }

    #[test]
    fn poisson_mean_is_close() {
        let mut rng = SynthRng::seed_from_u64(0);
        for mean in [0.5, 4.0, 12.0, 50.0] {
            let total: u64 = (0..20_000).map(|_| poisson(&mut rng, mean)).sum();
            #[allow(clippy::cast_precision_loss, reason = "small test values")]
            let observed = total as f64 / 20_000.0;
            assert!(
                (observed - mean).abs() < mean * 0.05 + 0.05,
                "{mean}: {observed}"
            );
        }
    }

    #[test]
    fn sample_is_distinct() {
        let mut rng = SynthRng::seed_from_u64(0);
        let mut picked = sample(&mut rng, 100, 40);
        picked.sort_unstable();
        picked.dedup();
        assert_eq!(picked.len(), 40);
    }
}
