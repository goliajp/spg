//! no_std floating-point helpers (trunc / sqrt / exp / ln / powi /
//! round / ceil / floor — bit-twiddling or Newton-iteration
//! reimplementations of the `std`/libm methods unavailable under
//! `no_std`) plus the process-static xorshift64* PRNG behind
//! `random()` / `gen_random_uuid()`. The `f64_sqrt` / `f64_ceil` /
//! `f64_floor` trio stays `pub(crate)` (re-exported from eval) for
//! the aggregate stddev / percentile paths in `aggregate.rs`.
//! Split out of `eval.rs` (cut 27).

/// no_std-compatible `trunc(x)` for f64 — truncate toward zero.
/// `as i64 as f64` already truncates toward zero for the in-range
/// case; the |x| > 2^53 branch returns x verbatim because the f64
/// is already integer-precision.
pub(super) fn f64_trunc(x: f64) -> f64 {
    if x.is_nan() || x.is_infinite() {
        return x;
    }
    if x >= 9_007_199_254_740_992.0 || x <= -9_007_199_254_740_992.0 {
        return x;
    }
    (x as i64) as f64
}

/// xorshift64* PRNG state — process-static seed advanced on
/// every `random()` call. Not cryptographically secure; use
/// `gen_random_uuid` / future crypto-RNG functions when
/// security matters.
static PRNG_STATE: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0x2545_F491_4F6C_DD1D);

/// Advance the PRNG and return the raw next 64-bit state.
/// Shared between `random()` and `gen_random_uuid()`. The CAS
/// loop guarantees concurrent callers each see a distinct value
/// — important for `gen_random_uuid` collision freedom under
/// concurrent INSERTs.
pub(super) fn prng_next_u64() -> u64 {
    use core::sync::atomic::Ordering;
    let mut x = PRNG_STATE.load(Ordering::Relaxed);
    loop {
        if x == 0 {
            x = 0x2545_F491_4F6C_DD1D;
        }
        let mut next = x;
        next ^= next << 13;
        next ^= next >> 7;
        next ^= next << 17;
        match PRNG_STATE.compare_exchange_weak(x, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(seen) => x = seen,
        }
    }
}

/// v7.38 (read01 P6.08) — process-wide monotonic guard for `uuidv7`. Packs the
/// last-issued 48-bit millisecond and its 12-bit intra-millisecond counter as
/// `(ms << 12) | counter`. Given the current wall-clock `base_ms`, returns a
/// `(ms, counter)` pair strictly greater than every prior result so generated
/// UUIDs stay time-ordered even within one millisecond or if the clock steps
/// backward; the counter rolls into the next millisecond once it saturates.
static UUIDV7_MONO: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub(super) fn uuidv7_monotonic(base_ms: u64) -> (u64, u16) {
    use core::sync::atomic::Ordering;
    let mut packed = UUIDV7_MONO.load(Ordering::Relaxed);
    loop {
        let last_ms = packed >> 12;
        let last_ctr = (packed & 0xFFF) as u16;
        let (ms, ctr) = if base_ms > last_ms {
            (base_ms, 0)
        } else if last_ctr < 0xFFF {
            (last_ms, last_ctr + 1)
        } else {
            (last_ms + 1, 0)
        };
        let next = (ms << 12) | u64::from(ctr);
        match UUIDV7_MONO.compare_exchange_weak(packed, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return (ms, ctr),
            Err(seen) => packed = seen,
        }
    }
}

/// v7.37.17 (17.6 siblings) — Reseed the PRNG. PG's setseed(f)
/// accepts a value in [-1, 1] and uses it as the seed source.
/// We map that range into 64 bits deterministically.
pub(super) fn prng_seed(seed: f64) {
    use core::sync::atomic::Ordering;
    // Map [-1, 1] → u64. Use the raw f64 bit pattern so any
    // value in the range produces a distinct state. Force
    // non-zero so the xorshift doesn't get stuck.
    let bits = seed.to_bits();
    let state = if bits == 0 { 0x2545_F491_4F6C_DD1D } else { bits };
    PRNG_STATE.store(state, Ordering::Relaxed);
}

/// Advance the PRNG and return a uniform double in [0, 1).
pub(super) fn prng_next_f64() -> f64 {
    // 53 bits of randomness mapped to [0, 1).
    let mantissa = prng_next_u64() >> 11;
    let denom = (1u64 << 53) as f64;
    mantissa as f64 / denom
}

/// no_std `f64::sqrt(x)` — delegates to `libm::sqrt`, the
/// correctly-rounded IEEE-754 square root PG itself calls via C
/// libm. Perfect squares round-trip exactly and non-squares match
/// PG to the last ULP (the previous Newton iteration lost a ULP,
/// e.g. sqrt(2) = 1.414213562373095 vs PG 1.4142135623730951).
/// x must be non-negative (caller's contract; negatives → NaN).
pub(crate) fn f64_sqrt(x: f64) -> f64 {
    libm::sqrt(x)
}

/// no_std `f64::exp(x)` — e^x via range-reduction + Taylor
/// series. Adequate for power(), exp(), and pseudo-random-ish
/// scales the engine uses; ~1e-12 relative error in the
/// common range. (libm::exp was evaluated but is itself ~1 ULP
/// off PG's glibc exp on e.g. exp(1), so it is not a clean
/// drop-in win — kept the existing series.)
pub(super) fn f64_exp(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x > 709.0 {
        return f64::INFINITY;
    }
    if x < -745.0 {
        return 0.0;
    }
    // exp(x) = 2^k * exp(r) where r = x - k*ln(2), |r| <= ln(2)/2.
    const LN2: f64 = 0.6931471805599453;
    let k = f64_round_half_away(x / LN2) as i32;
    let r = x - (k as f64) * LN2;
    // Taylor series for exp(r): sum r^n / n!  (rapid for |r|<0.35)
    let mut term = 1.0;
    let mut sum = 1.0;
    for n in 1..=20 {
        term *= r / (n as f64);
        sum += term;
        if term.abs() < 1e-18 {
            break;
        }
    }
    // Multiply by 2^k.
    f64_powi(2.0, k) * sum
}

/// no_std `f64::ln(x)` — natural log via range-reduction +
/// atanh series. x must be positive (caller's contract).
pub(super) fn f64_ln(x: f64) -> f64 {
    if x <= 0.0 {
        return f64::NAN;
    }
    if x == 1.0 {
        return 0.0;
    }
    // x = 2^k * m where m in [0.5, 1.0). Then ln(x) = k*ln(2) + ln(m).
    const LN2: f64 = 0.6931471805599453;
    let mut k = 0i32;
    let mut m = x;
    while m >= 2.0 {
        m *= 0.5;
        k += 1;
    }
    while m < 1.0 {
        m *= 2.0;
        k -= 1;
    }
    // Now m in [1.0, 2.0). Use atanh series via u = (m-1)/(m+1).
    // ln(m) = 2*(u + u^3/3 + u^5/5 + ...). Converges fast.
    let u = (m - 1.0) / (m + 1.0);
    let u2 = u * u;
    let mut term = u;
    let mut sum = u;
    for k_iter in 1..50 {
        term *= u2;
        let denom = (2 * k_iter + 1) as f64;
        sum += term / denom;
        if (term / denom).abs() < 1e-18 {
            break;
        }
    }
    2.0 * sum + (k as f64) * LN2
}

/// no_std `f64::powi` substitute — integer exponent for f64
/// base. Uses repeated multiplication; correct for the small
/// exponents the rounding / cast code uses (scale up to ±38).
pub(super) fn f64_powi(base: f64, exp: i32) -> f64 {
    if exp == 0 {
        return 1.0;
    }
    let mut result = 1.0;
    let mut b = if exp > 0 { base } else { 1.0 / base };
    let mut e = exp.unsigned_abs();
    while e > 0 {
        if e & 1 == 1 {
            result *= b;
        }
        e >>= 1;
        if e > 0 {
            b *= b;
        }
    }
    result
}

/// no_std-compatible `round(x)` for f64 with half-away-from-zero
/// rule (PG NUMERIC semantic — NOT banker's rounding).
pub(super) fn f64_round_half_away(x: f64) -> f64 {
    if x.is_nan() || x.is_infinite() {
        return x;
    }
    if x >= 0.0 {
        f64_floor(x + 0.5)
    } else {
        f64_ceil(x - 0.5)
    }
}

/// no_std-compatible `ceil(x)` for f64. Same shape as
/// `f64_floor` but rounds toward +infinity for fractional
/// values. Negative fractions round toward zero
/// (ceil(-1.5) → -1, NOT -2).
pub(crate) fn f64_ceil(x: f64) -> f64 {
    if x.is_nan() || x.is_infinite() {
        return x;
    }
    if x >= 9_007_199_254_740_992.0 || x <= -9_007_199_254_740_992.0 {
        return x;
    }
    let trunc = (x as i64) as f64;
    if x > 0.0 && x != trunc {
        trunc + 1.0
    } else {
        trunc
    }
}

/// no_std-compatible `floor(x)` for f64. SPG's engine is
/// `#![no_std]` and can't call `f64::floor` directly (libm).
/// This handles the floor semantic manually:
///   * NaN / Inf passthrough.
///   * Values outside i64 range are already integer-precision.
///   * Negative non-integers floor toward -infinity (the
///     critical PG-canonical semantic).
pub(crate) fn f64_floor(x: f64) -> f64 {
    if x.is_nan() || x.is_infinite() {
        return x;
    }
    // f64 representation: any value with |x| > 2^53 is integer
    // precision (mantissa is 52 bits), so floor is identity.
    if x >= 9_007_199_254_740_992.0 || x <= -9_007_199_254_740_992.0 {
        return x;
    }
    let trunc = (x as i64) as f64;
    if x < 0.0 && x != trunc {
        trunc - 1.0
    } else {
        trunc
    }
}
