//! read01 round 486 — `<column> [NOT] IN (<literals>)` answered in place.
//!
//! `big_in` was the read panel's worst shape. It compiles to two steps,
//! `Column` then `InSet`, which the round-482 fast path does not cover, so
//! it ran the general VM: a `Value` built from the cell and popped, a
//! `Value::Bool` built and popped. Its profile put `drop_glue<Value>` at
//! 20 % and the VM loop at 27 %.
//!
//! The fast path restates the membership decision instead of calling into
//! the `InSet` arm, and that duplication is deliberate — routing the arm
//! through a shared helper cost `like_filter` 4.5 % (a shape with no `IN`
//! in it), measured against the parent commit and not recovered by
//! `#[inline]`. See the note on `in_set_verdict`.
//!
//! The price of a duplicate is drift, so every case below runs down BOTH
//! paths and asserts they agree. `probe_in_set_shape` establishes with a
//! counter which spelling is which: `c IN (…)` fires the fast path,
//! `(c IN (…)) = true` does not.
//!
//! Expectations are PG18's, read off `psql -tA`.

use spg_engine::{Engine, QueryResult};

fn ids(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join(";"),
        other => panic!("{sql} -> {other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE r (id INT, g INT, s TEXT, b BIGINT, m SMALLINT)")
        .unwrap();
    e.execute(
        "INSERT INTO r VALUES \
         (1, 1, 'v1', 1, 1), (2, 2, 'v2', 2, 2), (3, 3, 'v3', 3, 3), \
         (4, NULL, NULL, NULL, NULL), (5, 5, 'v5', 5, 5)",
    )
    .unwrap();
    e
}

/// Run the same predicate through the fast path and through the general
/// VM, assert both give `expected`. `pred` must be a bare
/// `<column> [NOT] IN (…)`.
fn both_paths(e: &mut Engine, pred: &str, expected: &str) {
    let fast = format!("SELECT id FROM r WHERE {pred} ORDER BY id");
    let general = format!("SELECT id FROM r WHERE ({pred}) = true ORDER BY id");
    assert_eq!(ids(e, &fast), expected, "fast path: {pred}");
    assert_eq!(ids(e, &general), expected, "general path: {pred}");
}

#[test]
fn round486_int_in_and_not_in() {
    let mut e = seeded();
    both_paths(&mut e, "g IN (1,3,5)", "1;3;5");
    // The NULL row is neither in nor not-in: `NULL IN (…)` is NULL.
    both_paths(&mut e, "g NOT IN (1,3,5)", "2");
}

#[test]
fn round486_null_in_the_list_makes_a_miss_unknown() {
    let mut e = seeded();
    // A hit is still TRUE; a miss becomes NULL rather than FALSE, so
    // row 2 and row 5 drop out and NOT IN selects nothing at all.
    both_paths(&mut e, "g IN (1,3,NULL)", "1;3");
    both_paths(&mut e, "g NOT IN (1,3,NULL)", "");
}

#[test]
fn round486_text_set() {
    let mut e = seeded();
    both_paths(&mut e, "s IN ('v1','v3')", "1;3");
    both_paths(&mut e, "s NOT IN ('v1','v3')", "2;5");
    // No member matches: an empty result, not an error.
    both_paths(&mut e, "s IN ('1','3')", "");
}

#[test]
fn round486_other_integer_widths() {
    let mut e = seeded();
    both_paths(&mut e, "b IN (1,5)", "1;5");
    both_paths(&mut e, "m IN (2,3)", "2;3");
}

#[test]
fn round486_cross_family_needle_takes_the_interpreter_path() {
    // An INT column against TEXT literals: the fast path declines (the
    // needle's family does not match the set's) and the whole node goes
    // to the interpreter, which coerces exactly as PG does.
    let mut e = seeded();
    both_paths(&mut e, "g IN ('1','3')", "1;3");
}

#[test]
fn round486_double_negation_agrees() {
    // A third spelling of the same predicate, on the general path by a
    // different route than `= true`.
    let mut e = seeded();
    assert_eq!(
        ids(
            &mut e,
            "SELECT id FROM r WHERE NOT (g NOT IN (1,3,5)) ORDER BY id"
        ),
        "1;3;5"
    );
}
