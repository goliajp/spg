//! r1047 — `SELECT DISTINCT <col> FROM t ORDER BY <col>` walks the
//! index and emits one row per key group, instead of normalizing and
//! hashing every row.
//!
//! The index's keys are canonical (r1039: representation equality IS
//! value equality — the property every seek already depends on), so one
//! key is one distinct value. On the release sweep's shape — 400,000
//! rows, 1,000 distinct values — the hash path priced at 21.3-22.7 ms
//! against PG18.4's 14.2-16.2, with an ablation floor of 14.8: the hash
//! has to touch ALL the rows, so no polish of the per-row cost reaches
//! a walk that touches each VALUE once.
//!
//! Semantics pinned against PG18.4 (live differential, 2026-08-16):
//!
//!  - NULL is one distinct value: exactly one output row, placed where
//!    the ORDER BY puts NULLs (last ASC, first DESC, explicit wins).
//!  - Which representation of a duplicate group is printed (`1.5` vs
//!    `1.50`) is PLAN-DEPENDENT in PG itself: the same table printed
//!    `1.5` under default order and `1.50` under NULLS FIRST. So the
//!    pin here is our own deterministic choice — the first row of the
//!    key group that passes the WHERE — not PG's coin flip.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE dw (id INT PRIMARY KEY, n NUMERIC, k INT)")
        .unwrap();
    // Two groups with MIXED representations (1.50/1.5 and 2.0/2.00),
    // two NULLs, and a row whose k is NULL — the PG probe's table.
    e.execute(
        "INSERT INTO dw VALUES (1,1.50,3),(2,1.5,3),(3,2.0,1),(4,NULL,2),\
         (5,NULL,1),(6,0.5,2),(7,2.00,NULL),(8,3,2)",
    )
    .unwrap();
    e.execute("CREATE INDEX dw_n ON dw (n)").unwrap();
    e.execute("CREATE INDEX dw_k ON dw (k)").unwrap();
    e
}

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                spg_storage::Value::Null => String::from("NULL"),
                v => spg_engine::eval::value_to_text(v),
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// The integer column: unique representations, so this is PG18.4's
/// output row for row — `1, 2, 3, NULL`.
#[test]
fn r1047_distinct_walk_matches_pg_where_pg_is_deterministic() {
    let mut e = engine();
    assert_eq!(
        rows(&mut e, "SELECT DISTINCT k FROM dw ORDER BY k"),
        ["1", "2", "3", "NULL"]
    );
    assert_eq!(
        rows(&mut e, "SELECT DISTINCT k FROM dw ORDER BY k DESC"),
        ["NULL", "3", "2", "1"]
    );
    assert_eq!(
        rows(&mut e, "SELECT DISTINCT k FROM dw ORDER BY k NULLS FIRST"),
        ["NULL", "1", "2", "3"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT DISTINCT k FROM dw ORDER BY k DESC NULLS LAST"
        ),
        ["3", "2", "1", "NULL"]
    );
}

/// Mixed representations: one row per VALUE, the representative being
/// the first row of the group. Deterministic here, plan-dependent in PG.
#[test]
fn r1047_a_duplicate_group_is_one_row() {
    let mut e = engine();
    assert_eq!(
        rows(&mut e, "SELECT DISTINCT n FROM dw ORDER BY n"),
        ["0.5", "1.50", "2.0", "3", "NULL"]
    );
    assert_eq!(
        rows(&mut e, "SELECT DISTINCT n FROM dw ORDER BY n DESC"),
        ["NULL", "3", "2.0", "1.50", "0.5"]
    );
}

/// A WHERE that knocks out a group's FIRST row: the group still
/// appears, represented by its first SURVIVING row — and a group whose
/// every row is knocked out disappears.
#[test]
fn r1047_the_filter_picks_the_representative() {
    let mut e = engine();
    assert_eq!(
        rows(
            &mut e,
            "SELECT DISTINCT n FROM dw WHERE id NOT IN (1, 3) ORDER BY n"
        ),
        ["0.5", "1.5", "2.00", "3", "NULL"]
    );
    // Both NULL rows filtered out: no NULL output row.
    assert_eq!(
        rows(
            &mut e,
            "SELECT DISTINCT n FROM dw WHERE id NOT IN (4, 5) ORDER BY n"
        ),
        ["0.5", "1.50", "2.0", "3"]
    );
}

/// The control: drop the index and the same queries take the hash
/// path. The two paths must agree byte for byte — an index changing
/// the answer is the one thing an index may never do.
#[test]
fn r1047_the_walk_agrees_with_the_hash_path() {
    let queries = [
        "SELECT DISTINCT n FROM dw ORDER BY n",
        "SELECT DISTINCT n FROM dw ORDER BY n DESC",
        "SELECT DISTINCT n FROM dw ORDER BY n NULLS FIRST",
        "SELECT DISTINCT k FROM dw ORDER BY k",
        "SELECT DISTINCT n FROM dw WHERE id NOT IN (1, 3) ORDER BY n",
    ];
    let mut walk = engine();
    let mut hash = engine();
    hash.execute("DROP INDEX dw_n").unwrap();
    hash.execute("DROP INDEX dw_k").unwrap();
    for sql in queries {
        assert_eq!(rows(&mut walk, sql), rows(&mut hash, sql), "{sql}");
    }
}

/// DISTINCT ON is a different operation and stays off the walk; so does
/// a projection that is not the order column. Both still answer.
#[test]
fn r1047_wider_distincts_stay_correct() {
    let mut e = engine();
    // DISTINCT over (n, k) pairs — PG18.4 counts 7: the 1.50/1.5 pair
    // collapses (same k), the 2.0/2.00 pair does NOT (k 1 vs NULL).
    // The first version of this pin said 6, by hand, wrongly — the
    // engine and PG agreed with each other and not with me.
    assert_eq!(
        rows(
            &mut e,
            "SELECT DISTINCT n FROM (SELECT n, k FROM dw) s ORDER BY n"
        ),
        ["0.5", "1.50", "2.0", "3", "NULL"]
    );
    let pairs = match e
        .execute("SELECT DISTINCT n, k FROM dw ORDER BY n")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => rows.len(),
        other => panic!("{other:?}"),
    };
    assert_eq!(pairs, 7, "distinct over the pair, not the key");
}
