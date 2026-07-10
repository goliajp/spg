//! Scalar-function argument acceptance on `NUMERIC` values, PG18
//! differential (v7.37.16 Slice B). Locks that scalar functions which
//! already accept a `double precision` / `Value::Float` argument ALSO
//! accept a `Value::Numeric` — matching PostgreSQL, where numeric→float8
//! is an implicit cast so `sin(numeric)`, `erf(numeric)`, `abs(numeric)`,
//! `make_time(.., numeric)`, `(numeric)::float8`, … all resolve.
//!
//! Why this was latent: many scalar arms funnel numeric args through the
//! Numeric-safe `value_to_f64` helper (sqrt / power / ln / log / exp /
//! round / trunc / ceil / floor already worked). But a batch of math /
//! datetime functions pattern-matched `Value::Float(f) => …` with an
//! inline int-widening ladder (`Int`/`SmallInt`/`BigInt`) and NO Numeric
//! arm, so a `Value::Numeric` (reachable today from `::numeric`, a
//! numeric column, or numeric arithmetic) hit the `other =>` error arm
//! and was rejected while the same call on a float column succeeded. The
//! fix adds a `Value::Numeric` arm (converting via the same
//! `scaled / 10^scale` widening `value_to_f64` uses) so acceptance
//! matches PG. Purely additive — Float/Int behavior is byte-identical.
//!
//! Ground truth captured from live PostgreSQL 18.4 on 2026-07-04 (mini
//! docker `spg-bench-postgres`, db `bench`). Float-valued results are
//! asserted within a 1e-9 absolute tolerance (embeds the PG reference
//! value; a rejection would surface as `<ERR>` and fail); exact-typed
//! results (numeric abs / interval / timestamp / text) assert on the
//! rendered value verbatim.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn render(v: &Value) -> String {
    match v {
        Value::Null => "<NULL>".into(),
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::SmallInt(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Float(x) => x.to_string(),
        Value::Text(s) => s.to_string(),
        other => format!("<UNEXP:{other:?}>"),
    }
}

/// Run `sql`, render the LAST projected column of the first row. Empty
/// result -> `<NOROWS>`, error -> `<ERR>`.
fn cell(eng: &mut Engine, sql: &str) -> String {
    match eng.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) => {
            if rows.is_empty() {
                return "<NOROWS>".into();
            }
            render(&rows[0].values[rows[0].values.len() - 1])
        }
        Ok(other) => format!("<NONROWS:{other:?}>"),
        Err(_) => "<ERR>".into(),
    }
}

/// Exact-render assertion.
fn ck(eng: &mut Engine, sql: &str, want: &str) {
    let got = cell(eng, sql);
    assert_eq!(
        got, want,
        "\n  SQL:  {sql}\n  want(PG18): {want}\n  got(SPG):   {got}"
    );
}

/// Float-tolerance assertion: `fn(numeric)` must equal the PG18 reference
/// value `pg` within 1e-9. Embeds `pg` as the differential ground truth
/// and simultaneously proves the numeric argument was accepted (a
/// rejection errors the whole statement -> `<ERR>` != `true`).
fn ckf(eng: &mut Engine, expr: &str, pg: &str) {
    let sql = format!("SELECT (abs(({expr}) - ({pg})) < 1e-9)::text");
    let got = cell(eng, &sql);
    assert_eq!(
        got, "true",
        "\n  expr: {expr}\n  want(PG18): {pg}\n  got(SPG):   {got}"
    );
}

fn eng() -> Engine {
    Engine::new()
}

// ---------------------------------------------------------------------
// Trig / hyperbolic family — each takes double precision in PG; numeric
// resolves via the implicit numeric→float8 cast.
// ---------------------------------------------------------------------
#[test]
fn trig_family_accepts_numeric() {
    let mut e = eng();
    ckf(&mut e, "sin(0.5::numeric)", "0.479425538604203");
    ckf(&mut e, "cos(0.5::numeric)", "0.8775825618903728");
    ckf(&mut e, "tan(0.5::numeric)", "0.5463024898437905");
    ckf(&mut e, "asin(0.5::numeric)", "0.5235987755982989");
    ckf(&mut e, "acos(0.5::numeric)", "1.0471975511965979");
    ckf(&mut e, "atan(0.5::numeric)", "0.4636476090008061");
    ckf(&mut e, "sinh(0.5::numeric)", "0.5210953054937474");
    ckf(&mut e, "cosh(0.5::numeric)", "1.1276259652063807");
    ckf(&mut e, "tanh(0.5::numeric)", "0.46211715726000974");
    ckf(&mut e, "asinh(0.5::numeric)", "0.48121182505960347");
    ckf(&mut e, "acosh(1.5::numeric)", "0.9624236501192069");
    ckf(&mut e, "atanh(0.5::numeric)", "0.5493061443340548");
    ckf(&mut e, "cot(0.5::numeric)", "1.830487721712452");
}

// ---------------------------------------------------------------------
// Degree-input trig + two-arg atan2 / atan2d.
// ---------------------------------------------------------------------
#[test]
fn degree_trig_and_atan2_accept_numeric() {
    let mut e = eng();
    ckf(&mut e, "sind(30::numeric)", "0.5");
    ckf(&mut e, "cosd(60::numeric)", "0.5");
    ckf(&mut e, "tand(45::numeric)", "1");
    ckf(&mut e, "cotd(45::numeric)", "1");
    ckf(&mut e, "asind(0.5::numeric)", "30");
    ckf(&mut e, "acosd(0.5::numeric)", "60");
    ckf(&mut e, "atand(1::numeric)", "45");
    ckf(
        &mut e,
        "atan2(1::numeric, 2::numeric)",
        "0.4636476090008061",
    );
    ckf(&mut e, "atan2d(1::numeric, 1::numeric)", "45");
}

// ---------------------------------------------------------------------
// erf / erfc (PG 17+).
// ---------------------------------------------------------------------
#[test]
fn erf_family_accepts_numeric() {
    let mut e = eng();
    ckf(&mut e, "erf(0.5::numeric)", "0.5204998778130465");
    ckf(&mut e, "erfc(0.5::numeric)", "0.4795001221869535");
}

// ---------------------------------------------------------------------
// abs(numeric) — PG returns NUMERIC (type + scale preserved), not float8.
// ---------------------------------------------------------------------
#[test]
fn abs_numeric_returns_numeric() {
    let mut e = eng();
    ck(&mut e, "SELECT (abs(2.5::numeric))::text", "2.5");
    ck(&mut e, "SELECT (abs((-3.75)::numeric))::text", "3.75");
    ck(&mut e, "SELECT (abs(0::numeric))::text", "0");
}

// ---------------------------------------------------------------------
// PRNG seeding / normal draw — accept numeric args (void / non-null).
// ---------------------------------------------------------------------
#[test]
fn prng_functions_accept_numeric() {
    let mut e = eng();
    // setseed returns void (NULL) — proves acceptance without erroring.
    ck(
        &mut e,
        "SELECT (setseed(0.5::numeric) IS NULL)::text",
        "true",
    );
    ck(
        &mut e,
        "SELECT (random_normal(0.0::numeric, 1.0::numeric) IS NOT NULL)::text",
        "true",
    );
}

// ---------------------------------------------------------------------
// make_time / make_timestamp / make_interval — the fractional-seconds
// argument is double precision in PG; numeric is accepted. Asserted by
// equality against the already-working float-seconds path (proves the
// numeric arg is both accepted AND produces the identical value), which
// is independent of SPG's internal time representation.
// ---------------------------------------------------------------------
#[test]
fn make_datetime_accepts_numeric_seconds() {
    let mut e = eng();
    ck(
        &mut e,
        "SELECT (make_time(8, 15, 23.5::numeric) = make_time(8, 15, 23.5))::text",
        "true",
    );
    ck(
        &mut e,
        "SELECT (make_timestamp(2020, 1, 1, 8, 15, 23.5::numeric) \
         = make_timestamp(2020, 1, 1, 8, 15, 23.5))::text",
        "true",
    );
    // make_interval is positional in SPG: (years, months, weeks, days,
    // hours, mins, secs). Numeric seconds must match the float path.
    ck(
        &mut e,
        "SELECT (make_interval(0, 0, 0, 0, 0, 0, 23.5::numeric) \
         = make_interval(0, 0, 0, 0, 0, 0, 23.5))::text",
        "true",
    );
}

// ---------------------------------------------------------------------
// Cast numeric -> double precision (implicit in PG, explicit here).
// ---------------------------------------------------------------------
#[test]
fn cast_numeric_to_float8() {
    let mut e = eng();
    ck(&mut e, "SELECT ((2.5::numeric)::float8)::text", "2.5");
    ckf(&mut e, "(12345.6789::numeric)::float8", "12345.6789");
}

// ---------------------------------------------------------------------
// Regression guard — the funnel-through-value_to_f64 functions (already
// Numeric-safe before Slice B) must keep accepting numeric. Corpus
// completeness so a future refactor can't silently drop them.
// ---------------------------------------------------------------------
#[test]
fn preexisting_numeric_safe_functions_still_accept() {
    let mut e = eng();
    ckf(&mut e, "sqrt(2::numeric)", "1.4142135623730951");
    ckf(
        &mut e,
        "power(2::numeric, 0.5::numeric)",
        "1.4142135623730951",
    );
    ckf(&mut e, "ln(2::numeric)", "0.6931471805599453");
    ckf(&mut e, "log(100::numeric)", "2");
    ckf(&mut e, "exp(1::numeric)", "2.718281828459045");
    ckf(&mut e, "cbrt(27::numeric)", "3");
    ckf(&mut e, "radians(180::numeric)", "3.141592653589793");
    // round / trunc / ceil / floor keep numeric-in / numeric-out.
    ck(&mut e, "SELECT (round(2.5::numeric))::text", "3");
    ck(&mut e, "SELECT (trunc(2.9::numeric))::text", "2");
    ck(&mut e, "SELECT (ceil(2.1::numeric))::text", "3");
    ck(&mut e, "SELECT (floor(2.9::numeric))::text", "2");
    // numrange already accepts numeric bounds.
    ck(
        &mut e,
        "SELECT (numrange(1.1::numeric, 2.2::numeric))::text",
        "[1.1,2.2)",
    );
}
