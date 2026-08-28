//! v7.39.2 — a binary string compares by bytes, and aggregates as bytes.
//!
//! MySQL's `X'41'` / `0x41` / `b'…'` lower onto a bytea cast here. Two
//! things followed from that and both were wrong:
//!
//! * The case fold applied to the TEXT side of the comparison only. So
//!   `0x61 = 'a'` answered 1 — both already lower — while `X'41' = 'A'`
//!   answered 0, because the fold turned 'A' into 0x61 and compared it
//!   against the byte 0x41. The half that worked is what hid it. MySQL
//!   9.7.2 answers 1 for `X'41' = 'A'` and 0 for `X'41' = 'a'`: the
//!   binary character set does not fold, so BOTH answers move together.
//!
//! * `string_agg` refused a bytea outright, on both dialects. PG18
//!   answers `string_agg(v, ',')` over bytea with a BYTEA — measured,
//!   `\x412c42` for 'A' and 'B' — and that is also what makes MySQL's
//!   `GROUP_CONCAT(X'41')` work.
//!
//! Rendering is pinned on the wire (`e2e_mysqlwire_query.rs`), where the
//! MySQL text row is built; the engine's own rendering is PG's and stays
//! PG's, which the `pg_typeof` row below holds in place.

use spg_engine::{Engine, QueryResult};

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode=''")
        .expect("enter the MySQL dialect");
    e
}

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) if !rows.is_empty() => {
            spg_engine::eval::value_to_text(&rows[0].values[0])
        }
        Ok(_) => "<none>".to_string(),
        Err(err) => panic!("{sql}: {err}"),
    }
}

#[test]
fn a_binary_string_compares_without_folding_case() {
    let mut e = mysql();
    for (sql, want) in [
        ("SELECT X'41'='A'", "true"),
        ("SELECT 'A'=X'41'", "true"),
        // The other half of the same rule: no fold means 'a' does NOT
        // match. Pinning only the row above would let a blanket
        // "binary always equals" pass.
        ("SELECT X'41'='a'", "false"),
        ("SELECT 0x61='a'", "true"),
        ("SELECT 0x61='A'", "false"),
        ("SELECT X'4142' IN ('AB','x')", "true"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

#[test]
fn a_binary_string_in_a_where_clause_finds_the_row() {
    let mut e = mysql();
    e.execute("CREATE TABLE b (s VARCHAR(4))").expect("create");
    e.execute("INSERT INTO b VALUES ('A'),('a')")
        .expect("insert");
    assert_eq!(one(&mut e, "SELECT COUNT(*) FROM b WHERE s = X'41'"), "1");
}

#[test]
fn string_agg_over_bytea_answers_bytea_on_the_pg_side() {
    // PG18, measured: `\x412c42` and `bytea`. The engine's rendering is
    // PG's on both dialects — the MySQL wire is what turns those bytes
    // into `A,B`.
    let mut e = Engine::new();
    assert_eq!(
        one(
            &mut e,
            "SELECT string_agg(v, ',') FROM (SELECT '\\x41'::bytea v UNION ALL SELECT '\\x42'::bytea) z"
        ),
        "\\x412c42"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_typeof(string_agg(v, ',')) FROM (SELECT '\\x41'::bytea v) z"
        ),
        "bytea"
    );
    // PG's own separator overload takes a bytea. Its bytes must reach
    // the join, not be dropped for the empty default. `\xFF` is not a
    // character, so a separator that had gone through a text detour
    // could not come back looking like this.
    assert_eq!(
        one(
            &mut e,
            "SELECT string_agg(v, '\\xFF'::bytea) FROM (SELECT '\\x41'::bytea v UNION ALL SELECT '\\x42'::bytea) z"
        ),
        "\\x41ff42"
    );
    // DISTINCT reads the separator from a different place than the plain
    // shape above — the per-group snapshot, not the per-row list. Both
    // were threaded, and each turns this file red on its own.
    assert_eq!(
        one(
            &mut e,
            "SELECT string_agg(DISTINCT v, '\\xFF'::bytea) FROM (SELECT '\\x41'::bytea v UNION ALL SELECT '\\x42'::bytea) z"
        ),
        "\\x41ff42"
    );
}

#[test]
fn a_table_built_from_the_aggregate_gets_a_bytea_column() {
    // The STATIC type, which is a separate answer from the value's: with
    // it left at TEXT the whole statement fails, "expected TEXT, got
    // BYTEA", and the table is created with the wrong column type.
    let mut e = Engine::new();
    e.execute("CREATE TABLE src (v bytea)").expect("create");
    e.execute("INSERT INTO src VALUES ('\\x41'),('\\x42')")
        .expect("insert");
    e.execute("CREATE TABLE ct AS SELECT string_agg(v, ',') AS a FROM src")
        .expect("ctas");
    assert_eq!(
        one(
            &mut e,
            "SELECT data_type FROM information_schema.columns WHERE table_name='ct' AND column_name='a'"
        ),
        "bytea"
    );
    assert_eq!(one(&mut e, "SELECT a FROM ct"), "\\x412c42");
}

#[test]
fn string_agg_over_text_is_unchanged() {
    // The control: rewriting the join to build bytes must not change
    // what a text aggregate answers, separator and ordering included.
    let mut e = Engine::new();
    assert_eq!(
        one(
            &mut e,
            "SELECT string_agg(v, '-' ORDER BY v) FROM (SELECT 'b' v UNION ALL SELECT 'a') z"
        ),
        "a-b"
    );
    assert_eq!(
        one(&mut e, "SELECT string_agg(v, ',') FROM (SELECT 1 v) z"),
        "1"
    );
}
