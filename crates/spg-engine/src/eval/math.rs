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

/// xorshift64* PRNG behind `random()` / `gen_random_uuid()`. Not
/// cryptographically secure.
///
/// r1051 — the state is THREAD-LOCAL under `std`, matching PG's
/// per-backend `random()` semantics: `setseed()` and the test-mode
/// seed affect the calling session's stream and no one else's. The
/// first process-static version made `SPG_TEST_RANDOM_SEED` a
/// cross-test hazard the moment the merged e2e binary ran in
/// parallel (a seeded engine reset the stream under a concurrently
/// running uuid-uniqueness test), which is the same defect PG's
/// per-backend design exists to prevent. Streams on distinct threads
/// start from distinct golden-ratio-salted states, so
/// `gen_random_uuid` keeps its cross-thread collision freedom.
///
/// Under `no_std` there are no threads to separate; the process
/// static remains.
const PRNG_SENTINEL: u64 = 0x2545_F491_4F6C_DD1D;

#[cfg(feature = "std")]
extern crate std;

#[cfg(not(feature = "std"))]
static PRNG_STATE: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(PRNG_SENTINEL);

#[cfg(feature = "std")]
std::thread_local! {
    /// 0 = not yet initialised; first use salts it per thread.
    static PRNG_TL: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

/// Per-thread distinct starting states: golden-ratio steps of a
/// process counter, so two threads never begin on the same stream.
#[cfg(feature = "std")]
static PRNG_THREAD_SALT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0x9E37_79B9_7F4A_7C15);

fn xorshift(mut x: u64) -> u64 {
    if x == 0 {
        x = PRNG_SENTINEL;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    if x == 0 { PRNG_SENTINEL } else { x }
}

/// Advance the PRNG and return the raw next 64-bit state. Shared
/// between `random()` and `gen_random_uuid()`.
pub(super) fn prng_next_u64() -> u64 {
    #[cfg(feature = "std")]
    {
        PRNG_TL.with(|c| {
            let mut x = c.get();
            if x == 0 {
                use core::sync::atomic::Ordering;
                x = PRNG_SENTINEL
                    ^ PRNG_THREAD_SALT.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
            }
            let next = xorshift(x);
            c.set(next);
            next
        })
    }
    #[cfg(not(feature = "std"))]
    {
        use core::sync::atomic::Ordering;
        let mut x = PRNG_STATE.load(Ordering::Relaxed);
        loop {
            let next = xorshift(x);
            match PRNG_STATE.compare_exchange_weak(x, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return next,
                Err(seen) => x = seen,
            }
        }
    }
}

/// Set the calling session's PRNG state (setseed / test-mode seed).
fn prng_set_state(state: u64) {
    let state = if state == 0 { PRNG_SENTINEL } else { state };
    #[cfg(feature = "std")]
    PRNG_TL.with(|c| c.set(state));
    #[cfg(not(feature = "std"))]
    {
        use core::sync::atomic::Ordering;
        PRNG_STATE.store(state, Ordering::Relaxed);
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
        match UUIDV7_MONO.compare_exchange_weak(packed, next, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => return (ms, ctr),
            Err(seen) => packed = seen,
        }
    }
}

/// v7.37.17 (17.6 siblings) — Reseed the PRNG. PG's setseed(f)
/// accepts a value in [-1, 1] and uses it as the seed source.
/// We map that range into 64 bits deterministically.
pub(super) fn prng_seed(seed: f64) {
    // Map [-1, 1] → u64 via the raw bit pattern so any value in the
    // range produces a distinct state. Per-session under std (PG's
    // setseed scope), r1051.
    prng_set_state(seed.to_bits());
}

/// v7.38 元机制 D (r1051) — install a test-mode seed into the process
/// PRNG. `SPG_TEST_RANDOM_SEED` claimed to be "the single seed source
/// for every nondeterministic engine subsystem" while `random()` drew
/// from this process-static state untouched — the first `pin_v738_`
/// test caught the claim being false at the SQL level. Called from
/// `Engine::with_env_cfg` when (and only when) a seed is configured,
/// so production engines never pass through here; two seeded engines
/// interleaving draws still share one process state, which is the
/// documented test-mode caveat.
pub(crate) fn prng_install_seed(seed: u64) {
    prng_set_state(seed);
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

/// v7.39 (read01 round 79) — the transcendentals PG hands to the platform's
/// libm. PG's `exp`/`ln`/`sinh`… ARE the host C library's, so calling the same
/// thing is not an approximation of PG — it *is* PG's semantics, on the same
/// host. Under `no_std` (bare-metal embedders) the pure-Rust `libm` port is the
/// best available and is used instead.
///
/// What was here before: a hand-rolled range-reduction + Taylor series, with a
/// comment claiming "libm::exp was evaluated but is itself ~1 ULP off PG's exp
/// on e.g. exp(1), so it is not a clean drop-in win — kept the existing series."
/// Re-measured against live PG18.4 (rule: a note left behind can be wrong —
/// measure before you act on it): the series was off on SEVEN of nine probed
/// exp() inputs (exp(1), exp(2), exp(-1), exp(5), exp(-5), exp(10), exp(709))
/// and on ln(10), while the platform libm matched PG on every one of them. The
/// note was right that `libm` (the crate) is a ULP off on exp(1) — and stopped
/// there, keeping something far worse.
#[cfg(feature = "std")]
mod plat {
    pub fn exp(x: f64) -> f64 {
        x.exp()
    }
    pub fn ln(x: f64) -> f64 {
        x.ln()
    }
    pub fn pow(x: f64, y: f64) -> f64 {
        x.powf(y)
    }
    pub fn sinh(x: f64) -> f64 {
        x.sinh()
    }
    pub fn cosh(x: f64) -> f64 {
        x.cosh()
    }
    pub fn tanh(x: f64) -> f64 {
        x.tanh()
    }
    pub fn asinh(x: f64) -> f64 {
        x.asinh()
    }
    pub fn acosh(x: f64) -> f64 {
        x.acosh()
    }
    pub fn atanh(x: f64) -> f64 {
        x.atanh()
    }
}

#[cfg(not(feature = "std"))]
mod plat {
    pub fn exp(x: f64) -> f64 {
        libm::exp(x)
    }
    pub fn ln(x: f64) -> f64 {
        libm::log(x)
    }
    pub fn pow(x: f64, y: f64) -> f64 {
        libm::pow(x, y)
    }
    pub fn sinh(x: f64) -> f64 {
        libm::sinh(x)
    }
    pub fn cosh(x: f64) -> f64 {
        libm::cosh(x)
    }
    pub fn tanh(x: f64) -> f64 {
        libm::tanh(x)
    }
    pub fn asinh(x: f64) -> f64 {
        libm::asinh(x)
    }
    pub fn acosh(x: f64) -> f64 {
        libm::acosh(x)
    }
    pub fn atanh(x: f64) -> f64 {
        libm::atanh(x)
    }
}

pub(crate) fn f64_exp(x: f64) -> f64 {
    plat::exp(x)
}

pub(crate) fn f64_ln(x: f64) -> f64 {
    if x < 0.0 {
        return f64::NAN;
    }
    plat::ln(x)
}

/// x^y for FLOAT8. Was `exp(y * ln(x))`, which compounds the error of two
/// transcendentals; the platform's `pow` is a single correctly-rounded step.
pub(crate) fn f64_pow(x: f64, y: f64) -> f64 {
    plat::pow(x, y)
}

pub(crate) fn f64_sinh(x: f64) -> f64 {
    plat::sinh(x)
}
pub(crate) fn f64_cosh(x: f64) -> f64 {
    plat::cosh(x)
}
pub(crate) fn f64_tanh(x: f64) -> f64 {
    plat::tanh(x)
}
pub(crate) fn f64_asinh(x: f64) -> f64 {
    plat::asinh(x)
}
pub(crate) fn f64_acosh(x: f64) -> f64 {
    plat::acosh(x)
}
pub(crate) fn f64_atanh(x: f64) -> f64 {
    plat::atanh(x)
}

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

/// no_std-compatible `rint(x)` for f64 with the half-to-even (banker's) rule —
/// the semantic PG uses for FLOAT8 (`round(2.5::float8) → 2`,
/// `(-2.5)::float8::int → -2`), as opposed to NUMERIC's half-away-from-zero.
pub(crate) fn f64_round_half_even(x: f64) -> f64 {
    if x.is_nan() || x.is_infinite() {
        return x;
    }
    let fl = f64_floor(x);
    let diff = x - fl;
    if diff < 0.5 {
        fl
    } else if diff > 0.5 {
        fl + 1.0
    } else {
        // Exactly halfway → round to the even neighbour.
        let half = fl / 2.0;
        if half == f64_floor(half) {
            fl
        } else {
            fl + 1.0
        }
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
