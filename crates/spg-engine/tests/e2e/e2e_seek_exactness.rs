//! v7.38.19 — a seek that answered the whole predicate is not asked
//! again, and one that did not still is.
//!
//! Every seek in `index_access` returns a CANDIDATE set and the caller
//! re-applies the `WHERE` to each row. For the arms where the index
//! already decided the question, that re-check was the query's dominant
//! cost. Profiled on `count(*) FROM events WHERE project_id = 3` over
//! 200,000 rows: `try_index_seek` 1,814 leaf samples and
//! `binop::compare` 1,633 — and `compare`'s first arm is
//! `(Int, Int) => a.cmp(b)`, so it was never that a comparison is
//! expensive. It was that 25,000 of them were re-deciding what the walk
//! had decided.
//!
//! The danger is the other direction, and it is silent: an arm that
//! claims to be exact when it is over-approximate returns rows the
//! predicate rejects. So the tests below are ANSWER tests — every one of
//! them puts rows in the table that a candidate set could carry and the
//! predicate must not.
//!
//! Every count here is PostgreSQL 18.4's, run against the same fixtures
//! rather than reasoned about. That is not ceremony: the first draft of
//! `a_leading_prefix_walk_answers_exactly` asserted 30 where the answer
//! is 15, and it failed on the code being right.

use spg_engine::{Engine, QueryResult};

fn rows_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                spg_storage::Value::Text(t) => t.to_string(),
                spg_storage::Value::Null => "<NULL>".into(),
                other => format!("{other:?}"),
            })
            .collect(),
        other => panic!("expected rows from {sql}, got {other:?}"),
    }
}

fn one(e: &mut Engine, sql: &str) -> String {
    rows_of(e, sql).first().cloned().unwrap_or_default()
}

/// The shape the change exists for: a single equality on an indexed
/// integer column, where the seek IS the answer.
#[test]
fn a_single_equality_answers_exactly() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int PRIMARY KEY, g int NOT NULL, s text)")
        .unwrap();
    for i in 0..300i32 {
        e.execute(&format!("INSERT INTO t VALUES ({i}, {}, 'r{i}')", i % 10))
            .unwrap();
    }
    e.execute("CREATE INDEX t_g ON t (g)").unwrap();
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE g = 3"),
        "BigInt(30)"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE g = 99"),
        "BigInt(0)"
    );
    assert_eq!(
        rows_of(&mut e, "SELECT s FROM t WHERE g = 3 ORDER BY id LIMIT 3"),
        ["r3", "r13", "r23"]
    );
}

/// The composite prefix walk. Its candidates all share the LEADING key,
/// and the predicate names only that column — so the walk is the answer,
/// and the second column must not leak into it either way.
#[test]
fn a_leading_prefix_walk_answers_exactly() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int PRIMARY KEY, g int NOT NULL, k text NOT NULL)")
        .unwrap();
    for i in 0..300i32 {
        e.execute(&format!(
            "INSERT INTO t VALUES ({i}, {}, 'k{}')",
            i % 10,
            i % 4
        ))
        .unwrap();
    }
    e.execute("CREATE INDEX t_gk ON t (g, k)").unwrap();
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE g = 3"),
        "BigInt(30)"
    );
    // And with the second column named, the predicate is NOT the walk —
    // the candidates share `g` and must still be filtered by `k`. Thirty
    // rows carry `g = 3` and FIFTEEN of them carry `k = 'k3'`, which is
    // the whole point of the assertion: 30 would mean the second
    // conjunct was never applied.
    //
    // The 15 is PostgreSQL 18.4's, not mine. I wrote 30 from my own
    // arithmetic — `i % 4` alternates 3, 1 across the rows where
    // `i % 10 = 3` — and the test failed on the code being right.
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE g = 3 AND k = 'k3'"),
        "BigInt(15)"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE g = 3 AND k = 'k0'"),
        "BigInt(0)"
    );
    // Neither of those two discriminates, and that is worth saying: the
    // composite index covers BOTH columns, so the walk answers the whole
    // `AND` exactly either way. A residual on a column the index does
    // not cover is what separates them — ten of the thirty rows with
    // `g = 3` have `id < 100`, and claiming exactness answers thirty.
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE g = 3 AND id < 100"),
        "BigInt(10)"
    );
}

/// An `AND` is never exact: the seek picks ONE conjunct and drops the
/// rest, so its candidates include rows the other conjuncts reject.
/// This is the case that silently returns extra rows if the flag is
/// wrong, and the fixture is built so that it would.
#[test]
fn an_and_keeps_its_recheck() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int PRIMARY KEY, g int NOT NULL, h int NOT NULL)")
        .unwrap();
    for i in 0..300i32 {
        e.execute(&format!(
            "INSERT INTO t VALUES ({i}, {}, {})",
            i % 10,
            i % 7
        ))
        .unwrap();
    }
    e.execute("CREATE INDEX t_g ON t (g)").unwrap();
    // 30 rows have g = 3; of those, the ones with h = 5 are the answer.
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE g = 3"),
        "BigInt(30)"
    );
    let both = one(&mut e, "SELECT count(*) FROM t WHERE g = 3 AND h = 5");
    assert_ne!(
        both, "BigInt(30)",
        "the second conjunct must have been applied"
    );
    assert_eq!(both, "BigInt(4)");
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE g = 3 AND h > 100"),
        "BigInt(0)",
        "a conjunct nothing satisfies must empty the result"
    );
}

/// A range on an indexed integer column, both sides and one side.
#[test]
fn a_range_answers_exactly_and_its_bounds_are_the_bounds() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int PRIMARY KEY, g int NOT NULL)")
        .unwrap();
    for i in 0..300i32 {
        e.execute(&format!("INSERT INTO t VALUES ({i}, {i})"))
            .unwrap();
    }
    e.execute("CREATE INDEX t_g ON t (g)").unwrap();
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE g BETWEEN 10 AND 19"),
        "BigInt(10)"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE g > 295"),
        "BigInt(4)"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE g >= 295"),
        "BigInt(5)"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE g < 5"),
        "BigInt(5)"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE g <= 5"),
        "BigInt(6)"
    );
    // A range beside a second conjunct is an AND, and keeps its recheck.
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM t WHERE g BETWEEN 10 AND 19 AND id = 12"
        ),
        "BigInt(1)"
    );
}

/// An IN list, and the same list beside a conjunct it must not swallow.
#[test]
fn an_in_list_answers_exactly() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int PRIMARY KEY, g int NOT NULL)")
        .unwrap();
    for i in 0..300i32 {
        e.execute(&format!("INSERT INTO t VALUES ({i}, {})", i % 10))
            .unwrap();
    }
    e.execute("CREATE INDEX t_g ON t (g)").unwrap();
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE g IN (1, 2)"),
        "BigInt(60)"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE g IN (98, 99)"),
        "BigInt(0)"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM t WHERE g IN (1, 2) AND id < 50"
        ),
        "BigInt(10)"
    );
    // NOT IN is a different predicate and takes no seek.
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE g NOT IN (1, 2)"),
        "BigInt(240)"
    );
}

/// A jsonb containment answers correctly.
///
/// This one does NOT discriminate either, and I looked. Skipping the
/// recheck for every arm and re-running six shapes — array-contains-
/// scalar, array-contains-array, a nested object, an object nested
/// inside an array, and the numeric forms of both — changed no answer,
/// on SPG or on PostgreSQL 18.4. `jsonb_path_ops` hashes the whole
/// path-and-value, so for these the candidate set already IS the
/// answer.
///
/// The arm stays NOT exact anyway: its own comment says
/// over-approximate, and a shape I could not construct is not a shape
/// that does not exist. What this test checks is the answer.
#[test]
fn a_jsonb_containment_answers_correctly() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int PRIMARY KEY, d jsonb NOT NULL)")
        .unwrap();
    // Rows 0..99 all carry BOTH keys, so any key-based candidate set
    // holds all of them; only the VALUES separate the answer.
    for i in 0..100i32 {
        e.execute(&format!(
            "INSERT INTO t VALUES ({i}, '{{\"plan\":\"{}\",\"country\":\"{}\"}}')",
            if i % 2 == 0 { "pro" } else { "free" },
            if i % 3 == 0 { "jp" } else { "us" }
        ))
        .unwrap();
    }
    e.execute("CREATE INDEX t_d ON t USING gin (d jsonb_path_ops)")
        .unwrap();
    assert_eq!(
        one(
            &mut e,
            r#"SELECT count(*) FROM t WHERE d @> '{"plan":"pro"}'"#
        ),
        "BigInt(50)"
    );
    assert_eq!(
        one(
            &mut e,
            r#"SELECT count(*) FROM t WHERE d @> '{"plan":"pro","country":"jp"}'"#
        ),
        "BigInt(17)"
    );
    assert_eq!(
        one(
            &mut e,
            r#"SELECT count(*) FROM t WHERE d @> '{"plan":"nope"}'"#
        ),
        "BigInt(0)",
        "every row carries the KEY; only the value rejects them"
    );
}

/// A collated text column answers `=` correctly.
///
/// This one does NOT discriminate, and the reason is worth writing
/// down. `Collated::sort_key_of` appends the ORIGINAL bytes after the
/// ICU key, so two different strings never share a key — which makes
/// the collated equality probe exact in fact. It is still not CLAIMED
/// exact: the argument depends on an encoding detail two functions away
/// from the claim, and a conservative arm costs a re-check while a
/// wrong one costs rows. Recorded here so the next person does not read
/// this test as evidence of a recheck it never exercises.
#[test]
fn a_collated_text_equality_answers_correctly() {
    let mut e = Engine::new();
    e.execute("CREATE DATABASE c LC_COLLATE 'en_US.utf8'")
        .unwrap();
    e.execute("CREATE TABLE t (id int PRIMARY KEY, s text NOT NULL)")
        .unwrap();
    for (i, v) in ["abc", "ABC", "aBc", "abd"].iter().enumerate() {
        e.execute(&format!("INSERT INTO t VALUES ({i}, '{v}')"))
            .unwrap();
    }
    e.execute("CREATE INDEX t_s ON t (s)").unwrap();
    // `=` is byte equality under a deterministic collation, however the
    // three spellings sort.
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE s = 'abc'"),
        "BigInt(1)"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE s = 'ABC'"),
        "BigInt(1)"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE s = 'aBC'"),
        "BigInt(0)"
    );
}

/// Text under a byte-wise collation IS exact, and must still answer the
/// same.
#[test]
fn byte_wise_text_answers_exactly() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int PRIMARY KEY, s text NOT NULL)")
        .unwrap();
    for (i, v) in ["abc", "ABC", "aBc", "abd"].iter().enumerate() {
        e.execute(&format!("INSERT INTO t VALUES ({i}, '{v}')"))
            .unwrap();
    }
    e.execute("CREATE INDEX t_s ON t (s)").unwrap();
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE s = 'abc'"),
        "BigInt(1)"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE s = 'ABC'"),
        "BigInt(1)"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE s = 'zzz'"),
        "BigInt(0)"
    );
}

/// `a = 1 OR a = 2` unions two exact arms and is exact; `a = 1 OR b > 9`
/// unions an exact arm with an exact arm on another column and is too;
/// an OR with an unseekable side takes no seek at all.
#[test]
fn an_or_union_is_exact_only_when_both_halves_are() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int PRIMARY KEY, a int NOT NULL, b int NOT NULL)")
        .unwrap();
    for i in 0..300i32 {
        e.execute(&format!(
            "INSERT INTO t VALUES ({i}, {}, {})",
            i % 10,
            i % 6
        ))
        .unwrap();
    }
    e.execute("CREATE INDEX t_a ON t (a)").unwrap();
    e.execute("CREATE INDEX t_b ON t (b)").unwrap();
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE a = 1 OR a = 2"),
        "BigInt(60)"
    );
    // 30 with a = 0, 50 with b = 0, 10 with both.
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE a = 0 OR b = 0"),
        "BigInt(70)"
    );
    // One side unseekable: the whole predicate falls back to the scan
    // and must still be right.
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE a = 0 OR b * 2 = 4"),
        "BigInt(70)"
    );
}
