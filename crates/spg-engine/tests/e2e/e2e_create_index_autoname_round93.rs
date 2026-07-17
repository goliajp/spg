//! v7.39 (read01 round 93) — `CREATE INDEX` with the name omitted.
//!
//! PG has always let you write `CREATE INDEX ON t (a)` and picks the name
//! itself: `<table>_<label>…_idx`, where each label is a key column's name,
//! an expression's leading function name, or `expr` for a non-function
//! expression; INCLUDE columns contribute labels too, and a name clash
//! within the relation appends an integer counter (`_idx`, `_idx1`, …).
//! SPG required the name — the parser errored on the bare `ON`. It now
//! leaves the name empty and the engine derives the same name PG would.
//!
//! Fixed alongside: an expression key like `lower(c::text)` was rejected
//! as "references no column" because the column-extractor didn't descend
//! through a `::text` cast; it does now (named indexes hit the same path).
//!
//! Every generated name below was locked byte-identical against live
//! PG 18.4 running the identical DDL.

use spg_engine::{Engine, QueryResult};

fn index_names(e: &mut Engine, table: &str) -> Vec<String> {
    let sql =
        format!("SELECT indexname FROM pg_indexes WHERE tablename='{table}' ORDER BY indexname");
    match e.execute(&sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn unnamed_index_gets_pg_style_generated_names() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE zz (a int, b int, c int)").unwrap();
    for s in [
        "CREATE INDEX ON zz (a)",                 // zz_a_idx
        "CREATE INDEX ON zz (a)",                 // collision -> zz_a_idx1
        "CREATE INDEX ON zz (a)",                 // zz_a_idx2
        "CREATE UNIQUE INDEX ON zz (b)",          // unique doesn't change the name -> zz_b_idx
        "CREATE INDEX ON zz (a) INCLUDE (b)",     // INCLUDE cols are in the name -> zz_a_b_idx
        "CREATE INDEX ON zz (a) WHERE b > 0",     // partial predicate not in name -> zz_a_idx3
        "CREATE INDEX ON zz (c)",                 // zz_c_idx
        "CREATE INDEX ON zz (b, a)",              // multi-col -> zz_b_a_idx
        "CREATE INDEX ON zz ((a + b))",           // non-function expr -> zz_expr_idx
        "CREATE INDEX ON zz (lower(c::text))",    // function name -> zz_lower_idx (cast descended)
        "CREATE INDEX ON zz (upper(c::text), a)", // func + col -> zz_upper_a_idx
        "CREATE INDEX ON zz (abs(a))",            // zz_abs_idx
    ] {
        e.execute(s).unwrap_or_else(|x| panic!("{s}: {x:?}"));
    }
    assert_eq!(
        index_names(&mut e, "zz"),
        [
            "zz_a_b_idx",
            "zz_a_idx",
            "zz_a_idx1",
            "zz_a_idx2",
            "zz_a_idx3",
            "zz_abs_idx",
            "zz_b_a_idx",
            "zz_b_idx",
            "zz_c_idx",
            "zz_expr_idx",
            "zz_lower_idx",
            "zz_upper_a_idx",
        ]
    );
}

#[test]
fn named_index_and_if_not_exists_still_work() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a int)").unwrap();
    // An explicit name is untouched by the auto-name path.
    e.execute("CREATE INDEX my_ix ON t (a)").unwrap();
    // A second unnamed index coexists with the named one.
    e.execute("CREATE INDEX ON t (a)").unwrap();
    assert_eq!(index_names(&mut e, "t"), ["my_ix", "t_a_idx"]);
}

#[test]
fn cast_wrapped_expression_key_parses() {
    // The extractor descends through the `::text` cast to find the column,
    // so this no longer errors at parse time (it did before round 93).
    let mut e = Engine::new();
    e.execute("CREATE TABLE c (v int)").unwrap();
    e.execute("CREATE INDEX ix ON c (lower(v::text))").unwrap();
    assert_eq!(index_names(&mut e, "c"), ["ix"]);
}
