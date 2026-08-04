//! Round 713 — S05h probe ①: `ALTER TABLE … ALTER COLUMN … TYPE <ty>
//! COLLATE <name>` now re-collates the column. The type parser had
//! consumed the clause since Phase 2.5 and `parse_column_type_name`
//! discarded it: the statement succeeded and the ordering never changed —
//! the silent-divergence shape, worse than a syntax error.
//!
//! PG18 measurements (round 713): the clause re-collates (`a B c` under
//! en_US becomes `B a c` under C); an ABSENT clause RESETS to the type
//! default rather than keeping the old collation; a collation on a
//! non-collatable type is refused with `collations are not supported by
//! type bigint`. One recorded difference stands: PG's reset lands on ITS
//! database default (en_US.utf8 on the oracle), SPG's lands on C —
//! that is the datcollate difference the checklist §9 carries, not a gap
//! this surface can close.

use spg_engine::{Engine, QueryResult};

fn ordered(e: &mut Engine) -> String {
    match e.execute("SELECT t FROM c713 ORDER BY t").unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect::<Vec<_>>()
            .join(" "),
        other => panic!("{other:?}"),
    }
}

#[test]
fn round713_alter_column_type_collate_recollates() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE c713 (t TEXT COLLATE \"en_US\")").unwrap();
    e.execute("INSERT INTO c713 VALUES ('a'), ('B'), ('c')").unwrap();
    assert_eq!(ordered(&mut e), "a B c", "en_US case-blind order, as declared");
    // The clause re-collates: C is byte order.
    e.execute("ALTER TABLE c713 ALTER COLUMN t TYPE text COLLATE \"C\"")
        .unwrap();
    assert_eq!(ordered(&mut e), "B a c", "C order after the ALTER");
    // …and back.
    e.execute("ALTER TABLE c713 ALTER COLUMN t TYPE text COLLATE \"en_US\"")
        .unwrap();
    assert_eq!(ordered(&mut e), "a B c", "en_US again after the second ALTER");
    // No clause is a RESET to the type default (C here — the recorded
    // datcollate difference), not a keep of en_US.
    e.execute("ALTER TABLE c713 ALTER COLUMN t TYPE text").unwrap();
    assert_eq!(ordered(&mut e), "B a c", "absent clause resets, never keeps");
}

/// The S05h question that motivated the probe: with an INDEX on the
/// column, does re-collation leave a stale index order behind? ALTER
/// COLUMN TYPE rebuilds indices on the column, so the rebuilt index is
/// born under the new collation — the ordering stays right.
#[test]
fn round713_recollation_with_an_index_on_the_column() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE c713 (t TEXT COLLATE \"en_US\")").unwrap();
    e.execute("CREATE INDEX c713_t ON c713(t)").unwrap();
    e.execute("INSERT INTO c713 VALUES ('a'), ('B'), ('c')").unwrap();
    assert_eq!(ordered(&mut e), "a B c");
    e.execute("ALTER TABLE c713 ALTER COLUMN t TYPE text COLLATE \"C\"")
        .unwrap();
    assert_eq!(ordered(&mut e), "B a c", "no stale index order survives");
    // And range predicates (which may walk the index) agree with a
    // byte-order world: 'a' < 'c' keeps out 'B' (0x42).
    let n = match e
        .execute("SELECT count(*) FROM c713 WHERE t > 'a' AND t < 'c'")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{other:?}"),
    };
    assert_eq!(n, "0");
}

#[test]
fn round713_collation_on_a_non_collatable_type_is_refused() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE c713 (t TEXT, i INT)").unwrap();
    let err = format!(
        "{}",
        e.execute("ALTER TABLE c713 ALTER COLUMN i TYPE bigint COLLATE \"C\"")
            .expect_err("PG18 refuses a collation on bigint")
    );
    assert!(
        err.contains("collations are not supported by type bigint"),
        "{err}"
    );
    // The refusal happened before any rewrite: the column is intact.
    e.execute("INSERT INTO c713 VALUES ('x', 1)").unwrap();
}
