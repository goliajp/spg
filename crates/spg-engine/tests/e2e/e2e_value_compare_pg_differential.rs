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
//! DEFERRED (still error in SPG, tracked at the bottom of this file):
//!   * INET / CIDR / BIT ordering (`<` etc.) — PG's `network_cmp` /
//!     `varbit_cmp` compare a common prefix before the length; a plain
//!     field compare would silently return a non-PG answer, so only
//!     equality is wired.
//!   * NUMERIC[] / FLOAT8[] / INTERVAL[] `=` — value-based element
//!     comparators (not the derived `Ord` on the stored repr).
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

// ---- deferred: ordering that PG supports but SPG intentionally errors ----

#[test]
fn deferred_orderings_error() {
    let mut e = Engine::new();
    // INET / CIDR / BIT ordering is deferred; must ERROR (not silently
    // return a non-PG answer) until a PG-`network_cmp`/`varbit_cmp`
    // comparator is wired.
    assert_eq!(cell(&mut e, "SELECT '10.0.0.1'::inet < '10.0.0.2'::inet"), "<ERR>");
    assert_eq!(cell(&mut e, "SELECT '10.0.0.0/8'::cidr < '10.0.0.0/16'::cidr"), "<ERR>");
    assert_eq!(cell(&mut e, "SELECT '1010'::bit(4) < '1011'::bit(4)"), "<ERR>");
}
