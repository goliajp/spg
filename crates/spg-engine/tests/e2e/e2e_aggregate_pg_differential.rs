//! Aggregate-function PG18 differential corpus (6th differential sweep).
//!
//! Ground truth captured from live PostgreSQL 18.4 on 2026-07-04 over a
//! small seeded table (NULLs + duplicates + multiple groups). Each
//! `check` asserts SPG's rendered output against the value PG produced.
//!
//! BUGS FIXED in the accompanying commit and locked here:
//!   * min/max/mode over DATE / TIMESTAMP / SMALLINT / NUMERIC — the
//!     aggregate `value_cmp` fell through to `_ => Equal` for those
//!     types, so `min(date_col)` / `max(numeric_col)` returned the
//!     FIRST row instead of the extreme (see `min_max_typed_columns`).
//!   * ORDER BY over NUMERIC (top-level and aggregate-internal) — the
//!     ORDER BY comparator lacked a NUMERIC / cross-int-width /
//!     int-float arm and fell to a debug-string compare, sorting
//!     `12.50 < 5.25 < 99.99`. This corrupted `array_agg(x ORDER BY
//!     numeric)`, `string_agg(... ORDER BY numeric)`, and ordered-set
//!     aggregates (mode / percentile) over NUMERIC keys (see
//!     `ordered_set_and_orderby_numeric`).
//!
//! DOCUMENTED REPRESENTATION / SEMANTIC divergences (SPG asserts its
//! own form; PG value noted in a comment — NOT bugs):
//!   * avg/stddev/variance/corr return FLOAT; PG returns NUMERIC, so
//!     PG prints trailing zeros (`26.0000000000000000`) where SPG
//!     prints `26`. Values are equal.
//!   * array_agg(DISTINCT x) / string_agg(DISTINCT x): PG sorts the
//!     distinct set (dedup is sort-based); SPG keeps first-seen order.
//!     SQL leaves DISTINCT-without-ORDER-BY order UNSPECIFIED, so
//!     SPG's order is a valid implementation.
//!   * json_object_agg: PG inserts spaces (`{ "a" : 10 }`); SPG is
//!     compact (`{"a": 10}`). Same value.
//!
//! CLOSED GAP (v7.37.16 — was DEFERRED across the aggregate + window
//! differential sweeps, now fixed and asserted against live PG here):
//!   * sum(NUMERIC) / avg(NUMERIC) previously raised a TypeMismatch;
//!     PG returns an exact NUMERIC. Fixed with an exact i128-mantissa
//!     accumulator (scales aligned on the max, no f64) + a NUMERIC
//!     finalize; avg replicates PG's `select_div_scale` result scale
//!     (e.g. `avg('1.50')` → `1.50000000000000000000`, 20 fractional
//!     digits). The int/float sum fast path is byte-identical — the
//!     numeric arm only fires on `Value::Numeric` cells. See
//!     `sum_avg_numeric` below.

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
        Value::Json(s) => s.to_string(),
        Value::IntArray(a) => arr(a.iter().map(|o| o.map(|n| n.to_string()))),
        Value::BigIntArray(a) => arr(a.iter().map(|o| o.map(|n| n.to_string()))),
        Value::TextArray(a) => arr(a.iter().cloned()),
        other => format!("{other:?}"),
    }
}

fn arr<I: Iterator<Item = Option<String>>>(it: I) -> String {
    let mut s = String::from("{");
    for (i, e) in it.enumerate() {
        if i > 0 {
            s.push(',');
        }
        match e {
            Some(x) => s.push_str(&x),
            None => s.push_str("NULL"),
        }
    }
    s.push('}');
    s
}

fn cell(eng: &mut Engine, sql: &str) -> String {
    match eng.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) => {
            if rows.is_empty() {
                return "<NOROWS>".into();
            }
            let mut out = Vec::new();
            for r in &rows {
                let v = &r.values;
                out.push(render(&v[v.len() - 1]));
            }
            out.join("|")
        }
        Ok(other) => format!("<NONROWS:{other:?}>"),
        Err(e) => format!("<ERR:{e:?}>"),
    }
}

fn check(eng: &mut Engine, sql: &str, expect: &str) {
    let got = cell(eng, sql);
    assert_eq!(got, expect, "\n  SQL: {sql}\n  want: {expect}\n  got:  {got}");
}

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int, g text, x int, bx bigint, y float8, b bool, d date, s text)")
        .unwrap();
    for row in [
        "(1,'a',10, 100, 1.5,  true,  '2020-01-01','foo')",
        "(2,'a',20, 200, 2.5,  false, '2020-03-01','bar')",
        "(3,'a',NULL,NULL,NULL, NULL,  NULL,        NULL)",
        "(4,'b',30, 300, 3.5,  true,  '2019-12-31','baz')",
        "(5,'b',30, 300, 3.5,  true,  '2021-06-15','foo')",
        "(6,'c',40, 400, NULL, false, '2020-01-01',NULL)",
    ] {
        e.execute(&format!("INSERT INTO t VALUES {row}")).unwrap();
    }
    e
}

// ---- empty set: sum/avg/min/max/string_agg/bool/array → NULL; count → 0 ----
#[test]
fn empty_set_semantics() {
    let mut e = seed();
    check(&mut e, "SELECT sum(x) FROM t WHERE false", "<NULL>");
    check(&mut e, "SELECT count(*) FROM t WHERE false", "0");
    check(&mut e, "SELECT count(x) FROM t WHERE false", "0");
    check(&mut e, "SELECT avg(x) FROM t WHERE false", "<NULL>");
    check(&mut e, "SELECT min(x) FROM t WHERE false", "<NULL>");
    check(&mut e, "SELECT max(x) FROM t WHERE false", "<NULL>");
    check(&mut e, "SELECT string_agg(s,',') FROM t WHERE false", "<NULL>");
    check(&mut e, "SELECT bool_and(b) FROM t WHERE false", "<NULL>");
    check(&mut e, "SELECT bool_or(b) FROM t WHERE false", "<NULL>");
    check(&mut e, "SELECT array_agg(x) FROM t WHERE false", "<NULL>");
    check(&mut e, "SELECT max(s) FROM t WHERE false", "<NULL>");
}

// ---- NULL handling: sum/avg/count(x) skip NULLs, count(*) counts rows,
//      array_agg KEEPS NULLs, all-NULL group → NULL ----
#[test]
fn null_handling() {
    let mut e = seed();
    check(&mut e, "SELECT sum(x) FROM t", "130");
    check(&mut e, "SELECT count(x) FROM t", "5");
    check(&mut e, "SELECT count(*) FROM t", "6");
    check(&mut e, "SELECT array_agg(x ORDER BY id) FROM t", "{10,20,NULL,30,30,40}");
    // all-NULL group
    check(&mut e, "SELECT sum(x) FROM t WHERE id=3", "<NULL>");
    check(&mut e, "SELECT count(x) FROM t WHERE id=3", "0");
    check(&mut e, "SELECT avg(x) FROM t WHERE id=3", "<NULL>");
    check(&mut e, "SELECT bool_and(b) FROM t WHERE id=3", "<NULL>");
    // avg is FLOAT in SPG; PG NUMERIC prints 26.0000000000000000 (equal value)
    check(&mut e, "SELECT avg(x) FROM t", "26");
}

// ---- DISTINCT: dedup works. Ordering of DISTINCT-without-ORDER-BY is
//      SQL-unspecified; PG sorts, SPG keeps first-seen (documented). ----
#[test]
fn distinct() {
    let mut e = seed();
    check(&mut e, "SELECT count(DISTINCT x) FROM t", "4");
    check(&mut e, "SELECT sum(DISTINCT x) FROM t", "100");
    check(&mut e, "SELECT count(DISTINCT s) FROM t", "3");
    // avg(DISTINCT) FLOAT; PG 25.0000000000000000
    check(&mut e, "SELECT avg(DISTINCT x) FROM t", "25");
    // SEMANTIC: PG sorts distinct set -> {10,20,30,40,NULL} / bar,baz,foo.
    // SPG keeps first-seen order (valid: DISTINCT order is unspecified).
    check(&mut e, "SELECT array_agg(DISTINCT x) FROM t", "{10,20,NULL,30,40}");
    check(&mut e, "SELECT string_agg(DISTINCT s, ',') FROM t", "foo,bar,baz");
}

// ---- FILTER ----
#[test]
fn filter_clause() {
    let mut e = seed();
    check(&mut e, "SELECT count(*) FILTER (WHERE x>15) FROM t", "4");
    check(&mut e, "SELECT sum(x) FILTER (WHERE x>15) FROM t", "120");
    check(&mut e, "SELECT sum(x) FILTER (WHERE x>100) FROM t", "<NULL>");
    // FILTER + GROUP BY
    check(
        &mut e,
        "SELECT g||':'||(count(*) FILTER (WHERE b)) FROM t GROUP BY g ORDER BY g",
        "a:1|b:2|c:0",
    );
}

// ---- ORDER BY inside the aggregate ----
#[test]
fn order_by_inside_agg() {
    let mut e = seed();
    check(&mut e, "SELECT string_agg(s,',' ORDER BY s) FROM t", "bar,baz,foo,foo");
    check(&mut e, "SELECT array_agg(x ORDER BY x DESC) FROM t", "{NULL,40,30,30,20,10}");
    check(&mut e, "SELECT string_agg(x::text,',' ORDER BY x) FROM t", "10,20,30,30,40");
}

// ---- stddev / variance: sample (n-1) vs population (n) divisors,
//      single-row (samp → NULL, pop → 0). SPG returns FLOAT; PG NUMERIC. ----
#[test]
fn stddev_variance_family() {
    let mut e = seed();
    // var_pop = 104, var_samp = variance = 130 (exact integers here)
    check(&mut e, "SELECT var_pop(x) FROM t", "104");
    check(&mut e, "SELECT var_samp(x) FROM t", "130");
    check(&mut e, "SELECT variance(x) FROM t", "130");
    // stddev_pop = sqrt(104), stddev_samp = stddev = sqrt(130).
    // live PG18.4: stddev_samp == sqrt(130::float8) (verified equal),
    // whose shortest round-trip is 11.40175425099138 — the prior
    // 11.401754250991381 was a 1-ULP artifact of the old hand-rolled
    // Newton sqrt, now replaced by libm::sqrt.
    check(&mut e, "SELECT stddev_pop(x) FROM t", "10.198039027185569");
    check(&mut e, "SELECT stddev_samp(x) FROM t", "11.40175425099138");
    check(&mut e, "SELECT stddev(x) FROM t", "11.40175425099138");
    // single-row group: samp undefined -> NULL, pop -> 0
    check(&mut e, "SELECT stddev_samp(x) FROM t WHERE id=1", "<NULL>");
    check(&mut e, "SELECT var_samp(x) FROM t WHERE id=1", "<NULL>");
    check(&mut e, "SELECT stddev_pop(x) FROM t WHERE id=1", "0");
    // Float data exercises the operation-order sensitivity: variance
    // uses PG's `(N*Σx² - (Σx)²)/N²` form (float.c float8_var_pop),
    // so these match live PG18.4 bit-for-bit (y = 1.5,2.5,3.5,3.5).
    check(&mut e, "SELECT var_pop(y) FROM t", "0.6875");
    check(&mut e, "SELECT var_samp(y) FROM t", "0.9166666666666666");
    check(&mut e, "SELECT stddev_pop(y) FROM t", "0.82915619758885");
    check(&mut e, "SELECT stddev_samp(y) FROM t", "0.9574271077563381");
}

// ---- bool / bit aggregates ----
#[test]
fn bool_and_bit() {
    let mut e = seed();
    check(&mut e, "SELECT bool_and(b) FROM t", "false");
    check(&mut e, "SELECT bool_or(b) FROM t", "true");
    check(&mut e, "SELECT bool_and(b) FROM t WHERE g='b'", "true");
    check(&mut e, "SELECT every(b) FROM t WHERE g='b'", "true");
    check(&mut e, "SELECT bit_and(x) FROM t", "0");
    check(&mut e, "SELECT bit_or(x) FROM t", "62");
    check(&mut e, "SELECT bit_xor(x) FROM t", "54");
}

// ---- min/max over TEXT / DATE and (BUG FIX) DATE comparison ----
#[test]
fn min_max_text_date() {
    let mut e = seed();
    check(&mut e, "SELECT min(s) FROM t", "bar");
    check(&mut e, "SELECT max(s) FROM t", "foo");
    // BUG FIX: min/max over DATE previously returned the first row
    // (value_cmp had no Date arm). PG: 2019-12-31 / 2021-06-15.
    check(&mut e, "SELECT min(d)::text FROM t", "2019-12-31");
    check(&mut e, "SELECT max(d)::text FROM t", "2021-06-15");
}

// ---- BUG FIX: min/max over SMALLINT / NUMERIC / TIMESTAMP columns ----
#[test]
fn min_max_typed_columns() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t2 (id int, sm smallint, nm numeric(10,2), ts timestamp)")
        .unwrap();
    for row in [
        "(1, 30, 12.50, '2020-01-01 10:00:00')",
        "(2, 10, 99.99, '2019-06-15 08:30:00')",
        "(3, 20, 5.25,  '2021-12-31 23:59:59')",
        "(4, NULL, NULL, NULL)",
    ] {
        e.execute(&format!("INSERT INTO t2 VALUES {row}")).unwrap();
    }
    check(&mut e, "SELECT min(sm) FROM t2", "10");
    check(&mut e, "SELECT max(sm) FROM t2", "30");
    check(&mut e, "SELECT min(nm)::text FROM t2", "5.25");
    check(&mut e, "SELECT max(nm)::text FROM t2", "99.99");
    check(&mut e, "SELECT min(ts)::text FROM t2", "2019-06-15 08:30:00");
    check(&mut e, "SELECT max(ts)::text FROM t2", "2021-12-31 23:59:59");
}

// ---- BUG FIX: ordered-set aggregates + ORDER BY over a NUMERIC key ----
#[test]
fn ordered_set_and_orderby_numeric() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t3 (nm numeric(10,2))").unwrap();
    for v in ["12.50", "99.99", "5.25"] {
        e.execute(&format!("INSERT INTO t3 VALUES ({v})")).unwrap();
    }
    // Previously the ORDER BY comparator sorted NUMERIC by debug string
    // (12.50 < 5.25 < 99.99). PG value order: 5.25,12.50,99.99.
    check(
        &mut e,
        "SELECT string_agg(nm::text, ',' ORDER BY nm) FROM t3",
        "5.25,12.50,99.99",
    );
    // mode() ties resolve to the smallest under the sort; PG -> 5.25.
    check(&mut e, "SELECT (mode() WITHIN GROUP (ORDER BY nm))::text FROM t3", "5.25");
    // percentile_disc(0.5) over 3 sorted values -> 2nd -> 12.50.
    check(
        &mut e,
        "SELECT (percentile_disc(0.5) WITHIN GROUP (ORDER BY nm))::text FROM t3",
        "12.50",
    );
}

// ---- CLOSED GAP: sum(NUMERIC) / avg(NUMERIC) exact, PG18-matched ----
// Ground truth from live PG18 (2026-07-04): over numeric(10,2)
// {12.50, 99.99, 5.25, NULL} and numeric(12,3) {1.100, 2.200, 3.300}.
#[test]
fn sum_avg_numeric() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE nd (id int, g text, nm numeric(10,2), n3 numeric(12,3))")
        .unwrap();
    for row in [
        "(1,'a',12.50,1.100)",
        "(2,'a',99.99,2.200)",
        "(3,'b',5.25,3.300)",
        "(4,'b',NULL,NULL)",
    ] {
        e.execute(&format!("INSERT INTO nd VALUES {row}")).unwrap();
    }
    // sum(numeric) → exact NUMERIC at the column scale (was TypeMismatch).
    check(&mut e, "SELECT sum(nm)::text FROM nd", "117.74");
    check(&mut e, "SELECT sum(n3)::text FROM nd", "6.600");
    // avg(numeric) → NUMERIC at PG's select_div_scale display scale.
    // PG: 117.74/3 carries 16 fractional digits.
    check(&mut e, "SELECT avg(nm)::text FROM nd", "39.2466666666666667");
    // PG: 6.600/3 = 2.2 padded to 16 fractional digits.
    check(&mut e, "SELECT avg(n3)::text FROM nd", "2.2000000000000000");
    // Mixed-scale exact add: 117.74 + 6.600 = 124.340 (scale aligns to 3).
    check(&mut e, "SELECT (sum(nm)+sum(n3))::text FROM nd", "124.340");
    // avg of a single 1.50 → 20 fractional digits (PG's qweight rule:
    // equal leading digits push the result scale to 16+4).
    check(
        &mut e,
        "SELECT avg(x)::text FROM (VALUES (1.50::numeric)) v(x)",
        "1.50000000000000000000",
    );
    // avg of {1.50, 9.99} = 5.745 → 16 fractional digits.
    check(
        &mut e,
        "SELECT avg(x)::text FROM (VALUES (1.50::numeric),(9.99::numeric)) v(x)",
        "5.7450000000000000",
    );
    // Empty / all-NULL numeric group → NULL (not 0), preserved.
    check(&mut e, "SELECT sum(nm)::text FROM nd WHERE false", "<NULL>");
    check(&mut e, "SELECT avg(nm)::text FROM nd WHERE id=4", "<NULL>");
    // GROUP BY numeric sum per group: 'a' {12.50,99.99}=112.49,
    // 'b' {5.25,NULL}=5.25.
    check(
        &mut e,
        "SELECT sum(nm)::text FROM nd GROUP BY g ORDER BY g",
        "112.49|5.25",
    );
}

// ---- ordered-set + json over INT (already correct; regression lock) ----
#[test]
fn ordered_set_and_json_int() {
    let mut e = seed();
    check(&mut e, "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY x) FROM t", "30");
    check(&mut e, "SELECT (percentile_disc(0.5) WITHIN GROUP (ORDER BY x))::text FROM t", "30");
    check(&mut e, "SELECT (mode() WITHIN GROUP (ORDER BY x))::text FROM t", "30");
    // json_agg / jsonb_agg keep NULLs, insertion order via ORDER BY id
    check(&mut e, "SELECT json_agg(x ORDER BY id)::text FROM t", "[10, 20, null, 30, 30, 40]");
    check(&mut e, "SELECT jsonb_agg(x ORDER BY id)::text FROM t", "[10, 20, null, 30, 30, 40]");
}

// ---- GROUP BY: count / sum / avg per group ----
#[test]
fn group_by_combos() {
    let mut e = seed();
    check(
        &mut e,
        "SELECT g||':'||count(*)||':'||coalesce(sum(x)::text,'<NULL>') FROM t GROUP BY g ORDER BY g",
        "a:3:30|b:2:60|c:1:40",
    );
    // avg per group FLOAT; PG NUMERIC prints trailing zeros (equal value)
    check(
        &mut e,
        "SELECT g||':'||coalesce(avg(x)::text,'<NULL>') FROM t GROUP BY g ORDER BY g",
        "a:15|b:30|c:40",
    );
}

// ---- numeric widening: sum(int) -> bigint, sum(bigint) value ----
#[test]
fn numeric_widening() {
    let mut e = seed();
    check(&mut e, "SELECT sum(bx) FROM t", "1300"); // PG: numeric 1300
    check(&mut e, "SELECT sum(x) FROM t", "130"); // PG: bigint 130
}

// ---- regression family: corr / covar / regr_count over paired columns ----
#[test]
fn regression_family() {
    let mut e = seed();
    // corr(y,x) = 1 (perfectly linear on the non-NULL pairs)
    check(&mut e, "SELECT corr(y,x) FROM t", "1");
    check(&mut e, "SELECT covar_pop(y,x) FROM t", "6.875");
    check(&mut e, "SELECT covar_samp(y,x) FROM t", "9.166666666666666");
    check(&mut e, "SELECT regr_count(y,x) FROM t", "4");
}
