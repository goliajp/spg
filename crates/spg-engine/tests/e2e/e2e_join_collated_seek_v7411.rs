//! v7.40.11 — a join whose filter is an equality on an indexed TEXT
//! column returns no rows, on any database with a locale collation.
//!
//! Reported against 7.40.9 (§3.16) as "a join filtered on a UNIQUE
//! constraint's non-leading column loses rows", which is where they met
//! it. It is neither about UNIQUE nor about non-leading columns.
//! Measured here, one row in each table, joined and filtered:
//!
//! ```text
//!                                                      rows
//!   text column, plain CREATE INDEX, en_US.utf8          0   WRONG
//!   text column, no index at all                         1
//!   text column COLLATE "C", indexed                     1
//!   int column, indexed                                  1
//!   the same filter with no join                         1
//!   the same join with no filter                         1
//! ```
//!
//! So: an index, a text column keyed under a locale collation, a join,
//! and an equality filter. Creating the index is what changes the
//! answer — which is the shape this repository has now met on five
//! separate paths, and the reason `index_access::probe_key` exists.
//!
//! A locale-collated B-tree holds ICU sort keys. `probe_key` encodes a
//! probe the same way, so a seek asks the question the scan would; the
//! join peer's seek and the IN-list seek in `table_access` built their
//! keys straight from the value instead, landed in a space nothing
//! lives in, and reported no rows. `count(*)` agrees with the empty
//! answer, so nothing downstream can tell "none matched" from "they
//! were lost".
//!
//! The shipped images run `en_US.utf8`. Every fixture in this tree runs
//! under `C`, where a byte probe IS the right probe — which is why the
//! whole suite was green against this. The collation is declared here
//! rather than inherited, the same way `e2e_unique_index_collated_v7410`
//! does it.
//!
//! Their eleven-case narrowing is reproduced at the bottom, because
//! their V-numbers are how this defect will be referred to.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn shipped_collation_engine() -> Engine {
    let mut eng = Engine::new();
    assert!(
        eng.declare_database_collation("en_US.utf8")
            .expect("en_US.utf8 is a collation this build performs"),
        "the collation must take effect before any table exists"
    );
    eng
}

fn count(eng: &mut Engine, sql: &str) -> i64 {
    match eng.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
        QueryResult::Rows { rows, .. } => match rows[0].values[0] {
            Value::BigInt(n) => n,
            Value::Int(n) => i64::from(n),
            ref other => panic!("{sql}: {other:?}"),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

fn rows(eng: &mut Engine, sql: &str) -> usize {
    match eng.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
        QueryResult::Rows { rows, .. } => rows.len(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// One driving table, and a probe table in four configurations that
/// differ only in what the filtered column is and whether it is
/// indexed.
fn fixture() -> Engine {
    let mut eng = shipped_collation_engine();
    for sql in [
        "CREATE TABLE ja (id INT PRIMARY KEY)",
        "INSERT INTO ja VALUES (1)",
        // indexed text, database collation
        "CREATE TABLE jt (id INT PRIMARY KEY, a_id INT NOT NULL, k TEXT NOT NULL)",
        "CREATE INDEX jt_k ON jt (k)",
        "INSERT INTO jt VALUES (10, 1, 'k')",
        // the same, with no index
        "CREATE TABLE jn (id INT PRIMARY KEY, a_id INT NOT NULL, k TEXT NOT NULL)",
        "INSERT INTO jn VALUES (10, 1, 'k')",
        // the same, byte-ordered
        "CREATE TABLE jc (id INT PRIMARY KEY, a_id INT NOT NULL, k TEXT COLLATE \"C\" NOT NULL)",
        "CREATE INDEX jc_k ON jc (k)",
        "INSERT INTO jc VALUES (10, 1, 'k')",
        // an int column, indexed
        "CREATE TABLE ji (id INT PRIMARY KEY, a_id INT NOT NULL, k INT NOT NULL)",
        "CREATE INDEX ji_k ON ji (k)",
        "INSERT INTO ji VALUES (10, 1, 7)",
    ] {
        eng.execute(sql)
            .unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
    }
    eng
}

#[test]
fn the_row_is_returned_whether_or_not_the_column_is_indexed() {
    let mut eng = fixture();
    assert_eq!(
        rows(
            &mut eng,
            "SELECT b.k FROM jt b JOIN ja a ON a.id = b.a_id WHERE b.k = 'k'"
        ),
        1,
        "indexed text under a locale collation"
    );
    assert_eq!(
        rows(
            &mut eng,
            "SELECT b.k FROM jn b JOIN ja a ON a.id = b.a_id WHERE b.k = 'k'"
        ),
        1,
        "the control: the same table with no index"
    );
}

/// `count(*)` agrees with whatever the join produced, so it is the
/// assertion an application would have made and been lied to by.
#[test]
fn count_star_agrees_with_the_rows() {
    let mut eng = fixture();
    assert_eq!(
        count(
            &mut eng,
            "SELECT count(*) FROM jt b JOIN ja a ON a.id = b.a_id WHERE b.k = 'k'"
        ),
        1
    );
}

/// The two controls that name the condition: the collation, and the
/// type. Both were already right and must stay so — a fix that turns
/// the seek off for every text column would pass the test above and
/// cost every deployment its index.
#[test]
fn the_controls_that_name_the_condition() {
    let mut eng = fixture();
    assert_eq!(
        rows(
            &mut eng,
            "SELECT b.k FROM jc b JOIN ja a ON a.id = b.a_id WHERE b.k = 'k'"
        ),
        1,
        "COLLATE \"C\": a byte probe is the right probe"
    );
    assert_eq!(
        rows(
            &mut eng,
            "SELECT b.k FROM ji b JOIN ja a ON a.id = b.a_id WHERE b.k = 7"
        ),
        1,
        "an int column keys by value in every collation"
    );
}

/// Every join spelling, because the reporter measured five of them and
/// only `LIMIT` hid it.
#[test]
fn every_join_spelling() {
    let mut eng = fixture();
    let shapes: &[(&str, &str)] = &[
        (
            "inner",
            "SELECT b.k FROM jt b JOIN ja a ON a.id = b.a_id WHERE b.k = 'k'",
        ),
        (
            "left",
            "SELECT b.k FROM jt b LEFT JOIN ja a ON a.id = b.a_id WHERE b.k = 'k'",
        ),
        (
            "comma",
            "SELECT b.k FROM jt b, ja a WHERE a.id = b.a_id AND b.k = 'k'",
        ),
        (
            "order by",
            "SELECT b.k FROM jt b JOIN ja a ON a.id = b.a_id WHERE b.k = 'k' ORDER BY b.id",
        ),
        (
            "limit",
            "SELECT b.k FROM jt b JOIN ja a ON a.id = b.a_id WHERE b.k = 'k' LIMIT 8",
        ),
        (
            "pinned other side",
            "SELECT b.k FROM jt b JOIN ja a ON a.id = b.a_id WHERE b.k = 'k' AND a.id = 1",
        ),
    ];
    for (name, sql) in shapes {
        assert_eq!(rows(&mut eng, sql), 1, "{name}: {sql}");
    }
}

/// An IN list takes the other seek in the same file, and it builds its
/// key the same way.
#[test]
fn an_in_list_seeks_the_same_way() {
    let mut eng = fixture();
    assert_eq!(
        rows(
            &mut eng,
            "SELECT b.k FROM jt b JOIN ja a ON a.id = b.a_id WHERE b.k IN ('k', 'zzz')"
        ),
        1,
        "IN list, joined"
    );
    assert_eq!(
        rows(&mut eng, "SELECT k FROM jt WHERE k IN ('k', 'zzz')"),
        1,
        "IN list, unjoined — this one already worked"
    );
}

/// The reporter's own eleven cases, with their names, over their
/// schema: a table-level UNIQUE constraint and a filter on its second
/// column. Their V2/V3/VA are the three that answered.
#[test]
fn the_reported_narrowing_v1_through_vb() {
    let mut eng = shipped_collation_engine();
    for sql in [
        "CREATE TABLE v1a (id INT PRIMARY KEY)",
        "INSERT INTO v1a VALUES (1)",
        "CREATE TABLE v1b (id INT PRIMARY KEY, a_id INT NOT NULL, kind TEXT NOT NULL, \
         nm TEXT, UNIQUE (a_id, kind, nm))",
        "INSERT INTO v1b VALUES (10, 1, 'k', 'n1')",
    ] {
        eng.execute(sql)
            .unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
    }
    let cases: &[(&str, &str)] = &[
        (
            "V1",
            "SELECT b.nm FROM v1b b JOIN v1a a ON a.id = b.a_id WHERE b.kind = 'k'",
        ),
        (
            "V4",
            "SELECT b.nm FROM v1b b JOIN v1a a ON a.id = b.a_id WHERE b.kind = 'k' ORDER BY b.id",
        ),
        (
            "V5",
            "SELECT b.nm FROM v1b b LEFT JOIN v1a a ON a.id = b.a_id WHERE b.kind = 'k'",
        ),
        (
            "V6",
            "SELECT b.nm FROM v1b b, v1a a WHERE a.id = b.a_id AND b.kind = 'k'",
        ),
        (
            "V8",
            "SELECT b.nm FROM v1b b JOIN v1a a ON a.id = b.a_id WHERE b.kind = 'k' AND a.id = 1",
        ),
        (
            "VA",
            "SELECT b.nm FROM v1b b JOIN v1a a ON a.id = b.a_id WHERE b.kind = 'k' LIMIT 8",
        ),
        ("VB", "SELECT nm FROM v1b WHERE kind = 'k'"),
    ];
    for (name, sql) in cases {
        assert_eq!(rows(&mut eng, sql), 1, "{name}: {sql}");
    }
    assert_eq!(
        count(
            &mut eng,
            "SELECT count(*) FROM v1b b JOIN v1a a ON a.id = b.a_id WHERE b.kind = 'k'"
        ),
        1,
        "V7"
    );
    // And the third column of the constraint, which they did not list
    // and which fails the same way.
    assert_eq!(
        rows(
            &mut eng,
            "SELECT b.nm FROM v1b b JOIN v1a a ON a.id = b.a_id WHERE b.nm = 'n1'"
        ),
        1,
        "the constraint's third column"
    );
}

/// The wider half, which the reporter did not reach: the JOIN KEY
/// itself. `ON a.k = b.k` over indexed text probes the peer's index
/// once per driving row, and that probe asked the COLUMN's own
/// collation — a column that declares none inherits the database's, so
/// the entries were ICU sort keys and the probe was raw text.
///
/// ```text
///   SELECT count(*) FROM z1 a JOIN z2 b ON a.k = b.k
///     en_US.utf8, z2.k indexed     0
///     the same with no index       1
/// ```
///
/// An inner join over a text key is how anything joins on a natural
/// key — an email, a slug, a fingerprint, a token.
#[test]
fn a_text_join_key_finds_its_peer() {
    let mut eng = shipped_collation_engine();
    for sql in [
        "CREATE TABLE za (id INT PRIMARY KEY, k TEXT NOT NULL)",
        "CREATE TABLE zb (id INT PRIMARY KEY, k TEXT NOT NULL)",
        "CREATE INDEX zb_k ON zb (k)",
        "INSERT INTO za VALUES (1, 'k')",
        "INSERT INTO zb VALUES (2, 'k')",
        // the control: the same two tables with no index on the peer
        "CREATE TABLE zc (id INT PRIMARY KEY, k TEXT NOT NULL)",
        "INSERT INTO zc VALUES (3, 'k')",
    ] {
        eng.execute(sql)
            .unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
    }
    assert_eq!(
        count(&mut eng, "SELECT count(*) FROM za a JOIN zb b ON a.k = b.k"),
        1,
        "indexed text join key"
    );
    assert_eq!(
        count(&mut eng, "SELECT count(*) FROM za a JOIN zc c ON a.k = c.k"),
        1,
        "the control: no index, which already answered"
    );
    assert_eq!(
        rows(
            &mut eng,
            "SELECT a.id, b.id FROM za a LEFT JOIN zb b ON a.k = b.k WHERE b.id IS NOT NULL"
        ),
        1,
        "and a LEFT JOIN, which would have NULL-filled instead of dropping"
    );
}

/// The third site of the same defect, and the one that raises rather
/// than lying: a text FOREIGN KEY. The parent-existence check probed
/// the parent's B-tree with the raw value, so on a locale-collated
/// database an INSERT naming a parent that EXISTS was refused:
///
/// ```text
///   CREATE TABLE fk_p (k text PRIMARY KEY);
///   CREATE TABLE fk_c (id int PRIMARY KEY, pk text REFERENCES fk_p(k));
///   INSERT INTO fk_p VALUES ('a');
///   INSERT INTO fk_c VALUES (1, 'a');
///     en_US.utf8   ERROR: … violates foreign key constraint
///                  DETAIL: Key (pk)=(a) is not present in table "fk_p".
///     C            INSERT 0 1
/// ```
///
/// Measured on 7.40.10. A schema that keys on a text natural key — a
/// slug, an email, a token — could not be loaded at all.
#[test]
fn a_text_foreign_key_finds_its_parent() {
    let mut eng = shipped_collation_engine();
    for sql in [
        "CREATE TABLE fkp (k TEXT PRIMARY KEY)",
        "CREATE TABLE fkc (id INT PRIMARY KEY, pk TEXT REFERENCES fkp(k))",
        "INSERT INTO fkp VALUES ('a'), ('b')",
    ] {
        eng.execute(sql)
            .unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
    }
    eng.execute("INSERT INTO fkc VALUES (1, 'a')")
        .expect("a parent that exists is a parent that exists");
    assert_eq!(count(&mut eng, "SELECT count(*) FROM fkc"), 1);

    // And the half that must stay refused: a parent that is NOT there.
    let err = eng
        .execute("INSERT INTO fkc VALUES (2, 'zzz')")
        .expect_err("no such parent");
    let msg = format!("{err}");
    assert!(
        msg.contains("foreign key constraint"),
        "a missing parent is still a violation: {msg}"
    );

    // A NULL reference is allowed, as in PG.
    eng.execute("INSERT INTO fkc VALUES (3, NULL)")
        .expect("NULL is not a reference");
    assert_eq!(count(&mut eng, "SELECT count(*) FROM fkc"), 2);
}
