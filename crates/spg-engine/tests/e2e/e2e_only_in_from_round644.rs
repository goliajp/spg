//! v7.39 (round 644) — `FROM ONLY t` stopped being a no-op.
//!
//! Round 621 taught the parser to accept the keyword, which until then
//! read as a table NAMED `only` and failed on `relation "only" does not
//! exist`. It absorbed the keyword rather than carrying it, and said so:
//! SPG's children are separate relations that a plain scan does not
//! descend into, so ONLY already described what the scan did.
//!
//! That stopped being true when a partition parent started unioning its
//! children. Measured: `SELECT count(*) FROM ONLY <partitioned parent>`
//! answered 2 where PG18 answers 0 — the parent holds no rows of its
//! own, which is exactly what ONLY asks for.
//!
//! Two places had to learn it, and the second is the interesting one.
//! The parent expansion builds one synthetic CTE per parent NAME and
//! rewrites every reference carrying that name, so in
//! `FROM ONLY po a JOIN po b` the unqualified `b` put `po` on the list
//! and the rewrite then took BOTH — including the one that asked not to
//! descend. PG answers 0 for that join; SPG answered 2.
//!
//! Measured and NOT closed here, because each needs a field on a DML
//! statement struct and `UpdateStatement` carries a warning that round
//! 413 measured widening it in place overflowing the parser's nesting
//! stack — that is its own unit of work:
//!
//!   * `UPDATE ONLY t` and `DELETE FROM ONLY t` — closed in round 646,
//!     which added the field this round said it needed.
//!   * `TRUNCATE ONLY <partitioned>` is silently accepted as a no-op.
//!     PG refuses it: "cannot truncate only a partitioned table". The
//!     row outcome agrees; the refusal does not.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
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
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE po (k INT, v TEXT) PARTITION BY RANGE (k)")
        .unwrap();
    e.execute("CREATE TABLE po1 PARTITION OF po FOR VALUES FROM (0) TO (10)")
        .unwrap();
    e.execute("CREATE TABLE po2 PARTITION OF po FOR VALUES FROM (10) TO (20)")
        .unwrap();
    e.execute("INSERT INTO po VALUES (1,'a'), (11,'b')")
        .unwrap();
    e
}

#[test]
fn round644_only_does_not_descend_into_partitions() {
    let mut e = seeded();
    // The parent holds no rows of its own.
    assert_eq!(one(&mut e, "SELECT count(*) FROM ONLY po"), "0");
    // …and without the keyword it still answers for the whole tree.
    assert_eq!(one(&mut e, "SELECT count(*) FROM po"), "2");
}

/// The rewrite is keyed on the name, so a second unqualified reference
/// used to drag the ONLY one along with it.
#[test]
fn round644_only_survives_a_sibling_reference_to_the_same_parent() {
    let mut e = seeded();
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM ONLY po a JOIN po b ON a.k = b.k"
        ),
        "0"
    );
    // Both sides unqualified is the whole tree joined to itself.
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM po a JOIN po b ON a.k = b.k"),
        "2"
    );
    // …and ONLY on the right alone is equally empty.
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM po a JOIN ONLY po b ON a.k = b.k"
        ),
        "0"
    );
}

/// A table with no children is unaffected either way — ONLY is not a
/// filter, it is a decision about descending.
#[test]
fn round644_only_on_a_childless_table_changes_nothing() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE plain (a INT)").unwrap();
    e.execute("INSERT INTO plain VALUES (1), (2)").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM plain"), "2");
    assert_eq!(one(&mut e, "SELECT count(*) FROM ONLY plain"), "2");
    // A partition itself is a leaf: ONLY on one is the same scan.
    let mut e = seeded();
    assert_eq!(one(&mut e, "SELECT count(*) FROM ONLY po1"), "1");
    assert_eq!(one(&mut e, "SELECT count(*) FROM po1"), "1");
}

/// The pin that flipped. Round 644 recorded these as parse errors and
/// said the fix needed a field on a DML statement struct; round 646 added
/// it. On a partition parent, which holds no rows, both are a way of
/// asking for nothing — which is what PG answers too.
#[test]
fn round646_only_reaches_dml_now() {
    let mut e = seeded();
    e.execute("UPDATE ONLY po SET v = 'z'").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM po WHERE v = 'z'"), "0");
    e.execute("DELETE FROM ONLY po").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM po"), "2");
}
