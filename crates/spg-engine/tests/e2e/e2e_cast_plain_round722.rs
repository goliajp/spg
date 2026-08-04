//! Round 722 — a NAMED cast whose name is a plain scalar spelling
//! (`::NUMERIC`, `::REAL`, `numeric(10,2)`) resolves at COMPILE time
//! (`Step::CastPlain`) instead of falling to the interpreter per row.
//! The blanket Named→Subtree rule did worse than interpret: it made the
//! whole aggregate argument non-compilable, so `count(id::NUMERIC)`
//! missed the round-716 fused parallel lane entirely — the S07 family-②
//! cells (2.38-4.47× against PG, all ≤1.19× after).
//!
//! Answer pins, PG18-measured in the round-722 differential (10/11
//! byte-same; the 11th — `sum(id::REAL)` at 500k — is PG's own
//! order-sensitive float4 accumulation, §9-ledgered: PG answers two
//! DIFFERENT values for two spellings of it, SPG answers the exact sum
//! on all three of its paths).

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn round722_plain_named_casts_compile_and_answer_as_pg() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE c722 (id INT, s TEXT, t TIMESTAMP)").unwrap();
    e.execute(
        "INSERT INTO c722 SELECT gg, 'row' || gg, \
         TIMESTAMP '2020-01-01 00:00:00' + (gg % 9) * INTERVAL '1 day' \
         FROM generate_series(1, 100) gg",
    )
    .unwrap();
    for (sql, want) in [
        ("SELECT sum(id::NUMERIC) FROM c722", "5050"),
        // typmod rides through: numeric(10,2) keeps two decimal places.
        ("SELECT sum(id::NUMERIC(10,2)) FROM c722", "5050.00"),
        ("SELECT sum(id::NUMERIC / 5) FROM c722", "1010.00000000000000000000"),
        ("SELECT sum(id::REAL) FROM c722", "5050"),
        ("SELECT max((id % 2)::BOOLEAN::INT) FROM c722", "1"),
        ("SELECT count(DISTINCT t::DATE) FROM c722", "9"),
        ("SELECT min(s::VARCHAR(4)) FROM c722", "row1"),
        // A literal through the plain lane inside arithmetic.
        ("SELECT sum(('5')::NUMERIC + id) FROM c722 WHERE id <= 10", "105"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
    // The error path keeps the cast's own wording.
    let err = format!(
        "{}",
        e.execute("SELECT count(s::NUMERIC) FROM c722")
            .expect_err("text that is not a number refuses")
    );
    assert!(err.contains("invalid input syntax for type numeric"), "{err}");
}
