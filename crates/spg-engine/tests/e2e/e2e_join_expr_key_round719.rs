//! Round 719 — a single integer-only COMPUTED join key takes the i64
//! hash lane (S07 family ①). `ON a.id = b.id + 1` used to pay three
//! taxes the plain-column key did not: a canonical-string hash table,
//! the conjunct re-verified in `residual` against a materialised
//! combined row per matching pair, and an interpreted eval per build
//! row — 5.29× against PG on the 500k self-join, 0.70× after.
//!
//! These pins hold the lane's ANSWERS: every shape ran through the
//! round-719 differential against PG18 (12/12 byte-same, including
//! LEFT / RIGHT / FULL null-extension and the mixed-conjunct form
//! that must NOT take the lane).

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{other:?}"),
    }
}

fn seed(e: &mut Engine) {
    e.execute("CREATE TABLE j719 (id INT, g INT)").unwrap();
    e.execute("INSERT INTO j719 SELECT gg, gg % 3 FROM generate_series(1, 100) gg")
        .unwrap();
}

#[test]
fn round719_int_expr_key_joins_answer_as_pg() {
    let mut e = Engine::new();
    seed(&mut e);
    for (sql, want) in [
        // id = id + 1: 99 pairs (2..=100 each match one predecessor).
        ("SELECT count(*) FROM j719 a JOIN j719 b ON a.id = b.id + 1", "99"),
        ("SELECT count(*) FROM j719 a JOIN j719 b ON a.id = b.id - 3", "97"),
        // id = id * 2: even ids 2..=100 -> 50 pairs.
        ("SELECT count(*) FROM j719 a JOIN j719 b ON a.id = b.id * 2", "50"),
        // Anti-join: only a.id = 1 has no b with b.id + 1 = 1.
        (
            "SELECT count(*) FROM j719 a LEFT JOIN j719 b ON a.id = b.id + 1 \
             WHERE b.id IS NULL",
            "1",
        ),
        // RIGHT: only b.id = 100 has no a with a.id = 101.
        (
            "SELECT count(*) FROM j719 a RIGHT JOIN j719 b ON a.id = b.id + 1 \
             WHERE a.id IS NULL",
            "1",
        ),
        // A SECOND conjunct keeps the residual: id = id + 1 pairs whose
        // g also matches — id ≡ id+1 (mod 3) never holds, so zero.
        (
            "SELECT count(*) FROM j719 a JOIN j719 b ON a.id = b.id + 1 AND a.g = b.g",
            "0",
        ),
        // The winning row's VALUES, not just counts.
        ("SELECT sum(a.id) FROM j719 a JOIN j719 b ON a.id = b.id + 99", "100"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

/// NULLs on either side of the computed key join nothing (SQL `=`).
#[test]
fn round719_null_keys_never_match() {
    let mut e = Engine::new();
    seed(&mut e);
    e.execute("INSERT INTO j719 VALUES (NULL, 0), (NULL, 1)").unwrap();
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM j719 a JOIN j719 b ON a.id = b.id + 1"),
        "99",
        "NULL build keys and NULL probe keys both stay out"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM j719 a LEFT JOIN j719 b ON a.id = b.id + 1 \
             WHERE b.id IS NULL"
        ),
        "3",
        "the two NULL-id left rows null-extend, plus a.id = 1"
    );
}
