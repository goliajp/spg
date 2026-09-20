//! 9.0.0 — `unnest` over a non-array in a FROM.
//!
//! The refusal has three copies, and ablating the one in
//! `table_access.rs` changed nothing across 7,237 tests — so either a
//! shape existed that nobody writes, or the site was dead.
//!
//! It is LIVE. The shape that reaches it is unnest in a JOIN position
//! rather than as the primary FROM: `SELECT * FROM t, unnest(1)` goes
//! through `materialise_table_ref`, while `SELECT * FROM unnest(1)` and
//! `SELECT unnest(1)` each reach a different copy. Found by renaming
//! the message and asking seven spellings which one changed.

use spg_engine::Engine;

#[test]
fn a_non_array_in_a_from_names_the_type_postgresqls_way() {
    let mut e = Engine::new();
    // Measured on PG 18.6: `function unnest(integer) does not exist`.
    let err = e
        .execute("SELECT * FROM unnest(1)")
        .expect_err("a non-array is refused");
    assert!(
        format!("{err:?}").contains("function unnest(integer) does not exist"),
        "{err:?}"
    );
    // Each of these reaches a DIFFERENT copy of the same refusal, and
    // all three must answer the same sentence. The joined one is the
    // copy no test reached before.
    e.execute("CREATE TABLE u1 (i int)").expect("create");
    e.execute("INSERT INTO u1 VALUES (1)").expect("insert");
    for sql in [
        "SELECT unnest(1)",
        "SELECT * FROM u1, unnest(1)",
        "SELECT * FROM u1 JOIN unnest(1) ON true",
        "SELECT * FROM u1 CROSS JOIN unnest(2)",
    ] {
        let err = e.execute(sql).expect_err(sql);
        assert!(
            format!("{err:?}").contains("function unnest(integer) does not exist"),
            "{sql}: {err:?}"
        );
    }
}

#[test]
fn an_array_still_unnests_from_both_places() {
    let mut e = Engine::new();
    for sql in [
        "SELECT * FROM unnest(ARRAY[1,2])",
        "SELECT unnest(ARRAY[1,2])",
    ] {
        let spg_engine::QueryResult::Rows { rows, .. } = e
            .execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
        else {
            panic!("expected rows for {sql}");
        };
        assert_eq!(rows.len(), 2, "{sql}");
    }
}
