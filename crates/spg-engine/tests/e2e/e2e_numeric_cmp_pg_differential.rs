//! Mixed NUMERIC ↔ int / float comparison PG18 differential (v7.37.16
//! Slice A). Locks the parity of ORDER BY / min / max / window-min-max /
//! mode over a sort/aggregate key whose per-row values mix an exact
//! NUMERIC with a plain integer or float.
//!
//! Why this was latent: SPG evaluates `CASE WHEN k%2=0 THEN k ELSE k+0.5
//! END` per row, so even rows yield `Value::Int` and odd rows yield
//! `Value::Numeric` — the two `value_cmp` comparators (`orderby.rs` for
//! ORDER BY / window / mode, `aggregate.rs` for min / max) had NO arm for
//! the mixed pair and fell through to a debug-string sort (orderby) or
//! `_ => Equal` (aggregate). A min / max / ORDER BY over such a key was
//! silently mis-ordered. The added arms promote the integer to NUMERIC
//! (exact — mirrors `binop.rs` `numeric_or_widen`) and demote NUMERIC to
//! f64 against a float (PG `numeric op float8`), so comparison ordering
//! matches arithmetic + WHERE.
//!
//! Ground truth captured from live PostgreSQL 18.4 on 2026-07-04 (mini
//! docker `spg-bench-postgres`, db `bench`). PG unifies the CASE result
//! type (int→numeric, or numeric+float8→float8) so its ordering is the
//! true numeric order; SPG must produce the identical order despite the
//! heterogeneous per-row variants. `id`-based assertions render a plain
//! integer (no int/numeric/float text-representation ambiguity); the
//! `min/max::text` assertions use extremes whose winning representation is
//! unambiguous ("1.5" numeric, "6" integer).

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

/// Run `sql`, join the LAST projected column of every returned row with
/// `|`. Empty result -> `<NOROWS>`, error -> `<ERR>`.
fn cell(eng: &mut Engine, sql: &str) -> String {
    match eng.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) => {
            if rows.is_empty() {
                return "<NOROWS>".into();
            }
            rows.iter()
                .map(|r| render(&r.values[r.values.len() - 1]))
                .collect::<Vec<_>>()
                .join("|")
        }
        Ok(other) => format!("<NONROWS:{other:?}>"),
        Err(_) => "<ERR>".into(),
    }
}

fn ck(eng: &mut Engine, sql: &str, want: &str) {
    let got = cell(eng, sql);
    assert_eq!(got, want, "\n  SQL:  {sql}\n  want(PG18): {want}\n  got(SPG):   {got}");
}

/// `mix (id, k)`; the sort/aggregate key is
/// `CASE WHEN k%2=0 THEN k ELSE k+0.5 END` (even k -> Int, odd k ->
/// Numeric), producing values 4, 1.5, 5.5, 2, 3.5, 6 for ids 1..6.
fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE mix (id int, k int)").unwrap();
    for row in ["(1,4)", "(2,1)", "(3,5)", "(4,2)", "(5,3)", "(6,6)"] {
        e.execute(&format!("INSERT INTO mix (id,k) VALUES {row}")).unwrap();
    }
    e
}

// The mixed NUMERIC/int key expression (even -> Int, odd -> Numeric).
const VX: &str = "(CASE WHEN k%2=0 THEN k ELSE k+0.5 END)";
// The mixed NUMERIC/float key expression (even -> Float, odd -> Numeric).
const VF: &str = "(CASE WHEN k%2=0 THEN k::float8 ELSE k+0.5 END)";

#[test]
fn order_by_mixed_numeric_int() {
    let mut e = seed();
    // ORDER BY the mixed key: numeric value order (1.5,2,3.5,4,5.5,6),
    // not debug-string order. PG: 2|4|5|1|3|6.
    ck(&mut e, &format!("SELECT id FROM mix ORDER BY {VX} ASC, id"), "2|4|5|1|3|6");
    ck(&mut e, &format!("SELECT id FROM mix ORDER BY {VX} DESC, id"), "6|3|1|5|4|2");
}

#[test]
fn min_max_mixed_numeric_int() {
    let mut e = seed();
    // min = 1.5 (numeric, id2), max = 6 (int, id6). The old
    // `_ => Equal` aggregate fallback kept the first-arriving row.
    ck(&mut e, &format!("SELECT (min({VX}))::text FROM mix"), "1.5");
    ck(&mut e, &format!("SELECT (max({VX}))::text FROM mix"), "6");
    // Identify the extreme row by id (pure-integer render): the min/max
    // must land on id2 / id6.
    ck(
        &mut e,
        &format!("SELECT id FROM mix WHERE {VX} = (SELECT min({VX}) FROM mix) ORDER BY id"),
        "2",
    );
    ck(
        &mut e,
        &format!("SELECT id FROM mix WHERE {VX} = (SELECT max({VX}) FROM mix) ORDER BY id"),
        "6",
    );
}

#[test]
fn window_min_max_mixed_numeric_int() {
    let mut e = seed();
    // Window min/max over the whole partition (orderby::value_cmp path).
    ck(&mut e, &format!("SELECT (min({VX}) OVER ())::text FROM mix ORDER BY id LIMIT 1"), "1.5");
    ck(&mut e, &format!("SELECT (max({VX}) OVER ())::text FROM mix ORDER BY id LIMIT 1"), "6");
}

#[test]
fn order_by_and_minmax_mixed_numeric_float() {
    let mut e = seed();
    // NUMERIC vs float: PG demotes to float8; SPG demotes NUMERIC to f64.
    // Order and extremes are the same as the int mix.
    ck(&mut e, &format!("SELECT id FROM mix ORDER BY {VF} ASC, id"), "2|4|5|1|3|6");
    ck(
        &mut e,
        &format!("SELECT id FROM mix WHERE {VF} = (SELECT min({VF}) FROM mix) ORDER BY id"),
        "2",
    );
    ck(
        &mut e,
        &format!("SELECT id FROM mix WHERE {VF} = (SELECT max({VF}) FROM mix) ORDER BY id"),
        "6",
    );
}

#[test]
fn mode_mixed_numeric_int() {
    // mode() WITHIN GROUP is an ordered-set aggregate that dedups adjacent
    // equal values via value_cmp. Values: 2,4,3.5,3.5,2 -> 2 and 3.5 tie
    // at freq 2; PG breaks the tie to the smallest (2).
    let mut e = Engine::new();
    e.execute("CREATE TABLE mm (id int, k int)").unwrap();
    for row in ["(1,2)", "(2,4)", "(3,3)", "(4,3)", "(5,2)"] {
        e.execute(&format!("INSERT INTO mm (id,k) VALUES {row}")).unwrap();
    }
    ck(&mut e, &format!("SELECT (mode() WITHIN GROUP (ORDER BY {VX}))::text FROM mm"), "2");
}
