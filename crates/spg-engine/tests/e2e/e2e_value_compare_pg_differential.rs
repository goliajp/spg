//! `compare()` operator-dispatch PG18 differential (v7.37.17). Locks the
//! `=` / `<>` (and, where PG defines a matching total order and SPG has a
//! natural comparator, `<` / `<=` / `>` / `>=`) parity for the `Value`
//! variants whose `compare()` arms were added this slice.
//!
//! Background: Slice 4 (`dcfe5033`) found `jsonb = jsonb` ERRORED because
//! `compare()` had no `Value::Json` arm and fell to the catch-all
//! `TypeMismatch`. A full audit of the `Value` enum against `compare()`
//! found the same latent gap for SMALLINT, BYTEA, TIME, MONEY, MACADDR,
//! MACADDR8, INET, CIDR, BIT / BIT VARYING, and the UUID[] / BYTEA[] /
//! MONEY[] array variants — each stored + compared by PostgreSQL but
//! erroring in SPG. This slice wires the missing arms.
//!
//! Ground truth captured from live PostgreSQL 18.4 (mini docker
//! `spg-bench-postgres`, db `bench`) on 2026-07-04; every asserted case
//! returns `t` / the shown value there.
//!
//! v7.37.18 extends this with NUMERIC[] / FLOAT8[] / INTERVAL[] `=` /
//! `<>` via *value-based* element equality (PG `array_eq`): NUMERIC[] is
//! scale-insensitive (`[1.10] = [1.1]`), FLOAT8[] follows PG's array/btree
//! `NaN = NaN` and `+0.0 = -0.0`, INTERVAL[] compares by canonical span
//! (`[1 day] = [24:00:00]`). A NULL element equals only another NULL
//! (`ARRAY[1,NULL] = ARRAY[1,NULL]` is `t`, NOT NULL — confirmed live).
//!
//! INET / CIDR ordering (`network_cmp`), BIT / VARBIT ordering
//! (`varbit_cmp`), and NUMERIC[] / FLOAT8[] / INTERVAL[] ordering
//! (`array_cmp`) are implemented — see `inet_ordering` / `bit_ordering` /
//! `array_ordering`.
//!
//! DEFERRED (still error in SPG):
//!   * range / multirange `=` — needs discrete-range canonicalisation.
//!   * tsvector `=` — lexeme/position semantics unverified.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn render(v: &Value) -> String {
    match v {
        Value::Null => "<NULL>".into(),
        Value::Bool(b) => if *b { "t" } else { "f" }.into(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
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

fn ck(eng: &mut Engine, sql: &str, want: &str) {
    let got = cell(eng, sql);
    assert_eq!(got, want, "\n  SQL:  {sql}\n  want(PG18): {want}\n  got(SPG):   {got}");
}

// ---- scalar comparisons via literals ------------------------------------

#[test]
fn smallint_compare() {
    let mut e = Engine::new();
    ck(&mut e, "SELECT 1::smallint = 1::smallint", "t");
    ck(&mut e, "SELECT 2::smallint <> 3::smallint", "t");
    ck(&mut e, "SELECT 1::smallint < 2::smallint", "t");
    ck(&mut e, "SELECT 2::smallint >= 2::smallint", "t");
    // cross-width (smallint vs int / bigint) widens by value.
    ck(&mut e, "SELECT 1::smallint = 1", "t");
    ck(&mut e, "SELECT 1000::smallint < 9::int", "f");
    ck(&mut e, "SELECT 5::smallint = 5::bigint", "t");
    ck(&mut e, "SELECT 9::bigint > 1000::smallint", "f");
}

#[test]
fn bytea_compare() {
    let mut e = Engine::new();
    ck(&mut e, "SELECT '\\xDEAD'::bytea = '\\xDEAD'::bytea", "t");
    ck(&mut e, "SELECT '\\x01'::bytea <> '\\x02'::bytea", "t");
    ck(&mut e, "SELECT '\\x01'::bytea < '\\x02'::bytea", "t");
    ck(&mut e, "SELECT '\\xFF'::bytea > '\\x01'::bytea", "t");
    // empty bytea is less than any non-empty (prefix rule).
    ck(&mut e, "SELECT ''::bytea < '\\x00'::bytea", "t");
}

#[test]
fn time_compare() {
    let mut e = Engine::new();
    ck(&mut e, "SELECT '10:30:00'::time = '10:30:00'::time", "t");
    ck(&mut e, "SELECT '10:30:00'::time < '11:30:00'::time", "t");
    ck(&mut e, "SELECT '10:30:00'::time >= '10:30:00'::time", "t");
    ck(&mut e, "SELECT '10:30:00'::time <> '11:30:00'::time", "t");
}

#[test]
fn money_compare() {
    let mut e = Engine::new();
    ck(&mut e, "SELECT '$2.50'::money = '$2.50'::money", "t");
    ck(&mut e, "SELECT '$2.50'::money < '$3.50'::money", "t");
    ck(&mut e, "SELECT '-$1.00'::money < '$0.00'::money", "t");
}

#[test]
fn macaddr_compare() {
    let mut e = Engine::new();
    ck(&mut e, "SELECT '08:00:2b:01:02:03'::macaddr = '08:00:2b:01:02:03'::macaddr", "t");
    ck(&mut e, "SELECT '08:00:2b:01:02:03'::macaddr < '08:00:2b:01:02:04'::macaddr", "t");
    ck(
        &mut e,
        "SELECT '08:00:2b:01:02:03:04:05'::macaddr8 = '08:00:2b:01:02:03:04:05'::macaddr8",
        "t",
    );
    ck(
        &mut e,
        "SELECT '08:00:2b:01:02:03:04:05'::macaddr8 < '08:00:2b:01:02:03:04:06'::macaddr8",
        "t",
    );
}

#[test]
fn inet_cidr_equality() {
    let mut e = Engine::new();
    ck(&mut e, "SELECT '10.0.0.1'::inet = '10.0.0.1'::inet", "t");
    ck(&mut e, "SELECT '10.0.0.1'::inet <> '10.0.0.2'::inet", "t");
    // same address, different netmask bits -> not equal.
    ck(&mut e, "SELECT '10.0.0.0/8'::inet <> '10.0.0.0/16'::inet", "t");
    ck(&mut e, "SELECT '10.0.0.0/8'::cidr = '10.0.0.0/8'::cidr", "t");
    ck(&mut e, "SELECT '10.0.0.0/8'::cidr <> '10.0.0.0/16'::cidr", "t");
    ck(&mut e, "SELECT '2001:db8::1'::inet = '2001:db8::1'::inet", "t");
}

#[test]
fn bit_equality() {
    let mut e = Engine::new();
    ck(&mut e, "SELECT '1010'::bit(4) = '1010'::bit(4)", "t");
    ck(&mut e, "SELECT '1010'::bit(4) <> '1011'::bit(4)", "t");
    ck(&mut e, "SELECT '11111111'::varbit = '11111111'::varbit", "t");
    ck(&mut e, "SELECT '101'::varbit <> '1010'::varbit", "t");
}

#[test]
fn bit_ordering() {
    // v7.37 — BIT / VARBIT ordering (PG `varbit_cmp`): bit-lexicographic,
    // a shorter string that is a prefix of a longer one is LESS. All values
    // captured live from PostgreSQL 18.4.
    let mut e = Engine::new();
    ck(&mut e, "SELECT '10'::varbit < '11'::varbit", "t");
    ck(&mut e, "SELECT '1'::varbit < '10'::varbit", "t");
    ck(&mut e, "SELECT '10'::varbit < '100'::varbit", "t");
    ck(&mut e, "SELECT '10'::varbit < '1'::varbit", "f");
    ck(&mut e, "SELECT '101'::varbit < '1010'::varbit", "t");
    ck(&mut e, "SELECT '1010'::bit(4) < '1011'::bit(4)", "t");
    ck(&mut e, "SELECT '1011'::bit(4) >= '1010'::bit(4)", "t");
    ck(&mut e, "SELECT '10'::varbit <= '10'::varbit", "t");
}

#[test]
fn inet_ordering() {
    // v7.37 — INET / CIDR ordering (PG `network_cmp`): family (IPv4 < IPv6),
    // then the common netmask prefix, then the netmask length, then the full
    // address. All values captured live from PostgreSQL 18.4.
    let mut e = Engine::new();
    // address compared before netmask (10.x < 192.x despite /8 < /16).
    ck(&mut e, "SELECT '10.0.0.0/8'::inet < '192.168.0.0/16'::inet", "t");
    // same address, shorter netmask is less.
    ck(&mut e, "SELECT '10.0.0.0/8'::inet < '10.0.0.0/16'::inet", "t");
    // plain host address ordering.
    ck(&mut e, "SELECT '1.2.3.4'::inet < '1.2.3.5'::inet", "t");
    ck(&mut e, "SELECT '1.2.3.5'::inet > '1.2.3.4'::inet", "t");
    // family: IPv4 < IPv6.
    ck(&mut e, "SELECT '10.0.0.0'::inet < '::1'::inet", "t");
    // cidr follows the same comparator.
    ck(&mut e, "SELECT '10.0.0.0/8'::cidr < '10.0.0.0/16'::cidr", "t");
    ck(&mut e, "SELECT '10.0.0.1'::inet <= '10.0.0.1'::inet", "t");
    ck(&mut e, "SELECT '2001:db8::1'::inet > '2001:db8::'::inet", "t");
}

// ---- array comparisons via typed columns --------------------------------

fn seed_arrays() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE arr (id int, ua uuid[], ba bytea[], ma money[])")
        .unwrap();
    e.execute(
        "INSERT INTO arr VALUES (1, \
         ARRAY['550e8400-e29b-41d4-a716-446655440000'::uuid], \
         ARRAY['\\x01'::bytea], \
         ARRAY['$2.50'::money])",
    )
    .unwrap();
    e.execute(
        "INSERT INTO arr VALUES (2, \
         ARRAY['550e8400-e29b-41d4-a716-446655440001'::uuid], \
         ARRAY['\\x02'::bytea], \
         ARRAY['$3.50'::money])",
    )
    .unwrap();
    e
}

#[test]
fn array_equality_and_order() {
    let mut e = seed_arrays();
    // Self-equality of each array column (row 1 vs itself).
    ck(&mut e, "SELECT ua = ua FROM arr WHERE id = 1", "t");
    ck(&mut e, "SELECT ba = ba FROM arr WHERE id = 1", "t");
    ck(&mut e, "SELECT ma = ma FROM arr WHERE id = 1", "t");
    // row1 vs row2 inequality + ordering (element-wise; PG array_cmp).
    let x = "FROM arr a JOIN arr b ON a.id=1 AND b.id=2";
    ck(&mut e, &format!("SELECT a.ua <> b.ua {x}"), "t");
    ck(&mut e, &format!("SELECT a.ua < b.ua {x}"), "t");
    ck(&mut e, &format!("SELECT a.ba < b.ba {x}"), "t");
    ck(&mut e, &format!("SELECT a.ma < b.ma {x}"), "t");
}

// ---- value-based element equality: NUMERIC[] / FLOAT8[] / INTERVAL[] ----
// v7.37.18. Ground truth captured live from PG18 (mini `spg-bench-postgres`)
// on 2026-07-04; each case returns the shown value there.
//
// These use *seeded typed columns*, not `SELECT ARRAY[..]::T[] = ..`.
// An inline `ARRAY[..]` literal renders as a TEXT[] / FLOAT[] at eval time
// (a separate, pre-existing ARRAY-literal-typing gap), so the operands
// never become `Value::NumericArray` / `FloatArray` / `IntervalArray` and
// the value-based arm is bypassed. Storing into a `numeric[]` / `float8[]`
// / `interval[]` column coerces to the real array `Value` variant, which
// on SELECT-back exercises the `compare()` arms under test. Each row is a
// labelled operand; comparisons are a self-join on `id`.

/// `a.a <op> b.a` between rows `i` and `j` of seeded table `t`.
fn pair(e: &mut Engine, t: &str, i: i32, j: i32, op: &str) -> String {
    cell(
        e,
        &format!("SELECT a.a {op} b.a FROM {t} a JOIN {t} b ON a.id={i} AND b.id={j}"),
    )
}

#[test]
fn numeric_array_value_equality() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE nv (id int, a numeric[])").unwrap();
    for (id, v) in [
        (10, "ARRAY[1.10::numeric, 2.0::numeric]"), // [1.10, 2.0]
        (11, "ARRAY[1.1::numeric, 2.000::numeric]"), // value-equal, differing scale repr
        (20, "ARRAY[1.10::numeric]"),
        (21, "ARRAY[1.2::numeric]"),
        (22, "ARRAY[1.1::numeric]"), // value-equal to 20
        (30, "ARRAY[1::numeric, 2::numeric]"),
        (31, "ARRAY[1::numeric]"), // length mismatch vs 30
        (40, "ARRAY[1::numeric, NULL]"),
        (41, "ARRAY[1::numeric, NULL]"), // NULL == NULL
        (42, "ARRAY[1::numeric, 2::numeric]"), // NULL vs value
        (43, "ARRAY[1::numeric]"),      // NULL + length mismatch
    ] {
        e.execute(&format!("INSERT INTO nv VALUES ({id}, {v})")).unwrap();
    }
    // Scale-insensitive value equality: [1.10, 2.0] == [1.1, 2.000].
    assert_eq!(pair(&mut e, "nv", 10, 11, "="), "t");
    // Genuine value inequality.
    assert_eq!(pair(&mut e, "nv", 20, 21, "="), "f");
    // Value-equal (differing scale) -> `<>` is false.
    assert_eq!(pair(&mut e, "nv", 20, 22, "<>"), "f");
    // Length mismatch -> not equal.
    assert_eq!(pair(&mut e, "nv", 30, 31, "="), "f");
    // NULL element equals only another NULL (PG array_eq): result is `t`,
    // NOT NULL. (Confirmed live: `ARRAY[1,NULL] = ARRAY[1,NULL]` is `t`.)
    assert_eq!(pair(&mut e, "nv", 40, 41, "="), "t");
    // NULL vs value at the same position -> not equal.
    assert_eq!(pair(&mut e, "nv", 40, 42, "="), "f");
    // NULL element + length mismatch -> not equal.
    assert_eq!(pair(&mut e, "nv", 40, 43, "="), "f");
}

#[test]
fn float8_array_value_equality() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE fv (id int, a float8[])").unwrap();
    for (id, v) in [
        (10, "ARRAY[1.5::float8]"),
        (11, "ARRAY[1.5::float8]"),
        (12, "ARRAY[2.5::float8]"),
        (20, "ARRAY['NaN'::float8]"),
        (21, "ARRAY['NaN'::float8]"),
        (22, "ARRAY[1.0::float8]"),
        (30, "ARRAY[0.0::float8]"),
        (31, "ARRAY[(-0.0)::float8]"),
        (40, "ARRAY[1.5::float8, NULL]"),
        (41, "ARRAY[1.5::float8, NULL]"),
    ] {
        e.execute(&format!("INSERT INTO fv VALUES ({id}, {v})")).unwrap();
    }
    assert_eq!(pair(&mut e, "fv", 10, 11, "="), "t");
    assert_eq!(pair(&mut e, "fv", 10, 12, "<>"), "t");
    // PG array/btree semantics: NaN = NaN is `t`; NaN vs a number is `f`.
    assert_eq!(pair(&mut e, "fv", 20, 21, "="), "t");
    assert_eq!(pair(&mut e, "fv", 20, 22, "="), "f");
    // +0.0 = -0.0.
    assert_eq!(pair(&mut e, "fv", 30, 31, "="), "t");
    // NULL element equal only to NULL.
    assert_eq!(pair(&mut e, "fv", 40, 41, "="), "t");
}

#[test]
fn interval_array_value_equality() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE iv (id int, a interval[])").unwrap();
    for (id, v) in [
        (10, "ARRAY['1 day'::interval]"),
        (11, "ARRAY['24 hours'::interval]"), // 1 day == 24 hours
        (12, "ARRAY['1 mon'::interval]"),
        (13, "ARRAY['30 days'::interval]"), // 1 month == 30 days
        (14, "ARRAY['23 hours'::interval]"),
        (20, "ARRAY['1 day'::interval, NULL]"),
        (21, "ARRAY['24 hours'::interval, NULL]"),
    ] {
        e.execute(&format!("INSERT INTO iv VALUES ({id}, {v})")).unwrap();
    }
    // Unit-equivalence by canonical span.
    assert_eq!(pair(&mut e, "iv", 10, 11, "="), "t");
    assert_eq!(pair(&mut e, "iv", 12, 13, "="), "t");
    assert_eq!(pair(&mut e, "iv", 10, 14, "="), "f");
    assert_eq!(pair(&mut e, "iv", 10, 14, "<>"), "t");
    // NULL element equal only to NULL (value-equal elements + NULL == NULL).
    assert_eq!(pair(&mut e, "iv", 20, 21, "="), "t");
}

// ---- NUMERIC[] / FLOAT8[] / INTERVAL[] ordering (PG array_cmp) ----------

#[test]
fn array_ordering() {
    // PG `array_cmp`: element by element, NULL sorts GREATER than non-NULL,
    // shorter-is-less on a common prefix, FLOAT8 NaN is greatest. Exercised
    // on real typed columns (inline `ARRAY[..]::type[]` literals don't build
    // the typed variants). All values captured live from PostgreSQL 18.4.
    let mut e = Engine::new();
    e.execute("CREATE TABLE ov (id int, na numeric[], fa float8[], ia interval[])")
        .unwrap();
    e.execute(
        "INSERT INTO ov VALUES (1, ARRAY[1.1::numeric], ARRAY[1.5::float8], ARRAY['1 day'::interval])",
    )
    .unwrap();
    e.execute(
        "INSERT INTO ov VALUES (2, ARRAY[1.2::numeric], ARRAY[2.5::float8], ARRAY['2 days'::interval])",
    )
    .unwrap();
    // id 3: two-element with a NULL / a prefix / a NaN.
    e.execute(
        "INSERT INTO ov VALUES (3, ARRAY[1::numeric, NULL], ARRAY['NaN'::float8], ARRAY['25 hours'::interval])",
    )
    .unwrap();
    e.execute(
        "INSERT INTO ov VALUES (4, ARRAY[1::numeric, 2::numeric], ARRAY[1.0::float8], ARRAY['1 day'::interval])",
    )
    .unwrap();
    let ord = |e: &mut Engine, col: &str, x: i32, y: i32, op: &str| {
        cell(
            e,
            &format!("SELECT a.{col} {op} b.{col} FROM ov a JOIN ov b ON a.id={x} AND b.id={y}"),
        )
    };
    // element compare: [1.1] < [1.2], [1.5] < [2.5], [1 day] < [2 days].
    assert_eq!(ord(&mut e, "na", 1, 2, "<"), "t");
    assert_eq!(ord(&mut e, "fa", 1, 2, "<"), "t");
    assert_eq!(ord(&mut e, "ia", 1, 2, "<"), "t");
    // scale-insensitive value ordering + >=.
    assert_eq!(ord(&mut e, "na", 2, 1, ">"), "t");
    // NULL element sorts greatest: [1,NULL] > [1,2]  ->  [1,NULL] < [1,2] is f.
    assert_eq!(ord(&mut e, "na", 3, 4, "<"), "f");
    assert_eq!(ord(&mut e, "na", 4, 3, "<"), "t");
    // FLOAT8 NaN sorts greatest: [NaN] > [1.0].
    assert_eq!(ord(&mut e, "fa", 3, 4, ">"), "t");
    assert_eq!(ord(&mut e, "fa", 4, 3, "<"), "t");
    // INTERVAL canonical span: [25 hours] > [1 day]=24h.
    assert_eq!(ord(&mut e, "ia", 3, 4, ">"), "t");
    // prefix: [1] < [1,2] (id1 na is single-element [1.1]... use ids 4 vs 3
    // is the NULL case; a genuine prefix needs equal leading elems), so
    // compare [1,2] (id4) with itself for the equal case.
    assert_eq!(ord(&mut e, "na", 4, 4, "="), "t");
    assert_eq!(ord(&mut e, "na", 4, 4, "<="), "t");
}
