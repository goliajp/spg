//! 9.0.0 — `ALTER {VIEW|MATERIALIZED VIEW|TYPE} … RENAME TO` was a
//! silent no-op.
//!
//! Measured in a fresh schema against PostgreSQL 18.6: PG renames all
//! four of view / sequence / type / materialized view. SPG renamed the
//! SEQUENCE alone and reported success for the other three, so the old
//! object stayed where it was and the new name appeared nowhere in
//! `pg_class` / `pg_type`. A migration that renamed a view therefore
//! left both halves wrong and said nothing.
//!
//! Found while fixing `ALTER … OWNER TO`, out of the same
//! consume-to-boundary tail in the parser.
//!
//! The owner entry moves with the object: `object_owners` is keyed by
//! name, so leaving it behind would hand the renamed object the default
//! owner and hand the NEXT object of the old name this one's.

use spg_engine::{Engine, QueryResult};

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    rows.into_iter()
        .map(|r| spg_engine::eval::value_to_text(r.values.first().expect("one column")))
        .collect()
}

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    for sql in [
        "CREATE VIEW rv AS SELECT 1 AS a",
        "CREATE SEQUENCE rs",
        "CREATE TYPE rt AS (a int)",
        "CREATE MATERIALIZED VIEW rm AS SELECT 1 AS a",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
    e
}

#[test]
fn all_four_kinds_take_the_new_name() {
    let mut e = seeded();
    for sql in [
        "ALTER VIEW rv RENAME TO rv2",
        "ALTER SEQUENCE rs RENAME TO rs2",
        "ALTER TYPE rt RENAME TO rt2",
        "ALTER MATERIALIZED VIEW rm RENAME TO rm2",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
    assert_eq!(
        col(
            &mut e,
            "SELECT relname FROM pg_class WHERE relname IN ('rv','rv2','rs','rs2','rm','rm2') \
             ORDER BY 1"
        ),
        vec!["rm2".to_string(), "rs2".to_string(), "rv2".to_string()]
    );
    assert_eq!(
        col(
            &mut e,
            "SELECT typname FROM pg_type WHERE typname IN ('rt','rt2')"
        ),
        vec!["rt2".to_string()]
    );
    // And the renamed view still answers under its new name.
    assert_eq!(col(&mut e, "SELECT a FROM rv2"), vec!["1".to_string()]);
    assert_eq!(col(&mut e, "SELECT a FROM rm2"), vec!["1".to_string()]);
}

#[test]
fn the_owner_follows_the_rename() {
    let mut e = seeded();
    e.execute("CREATE USER r9 WITH PASSWORD 'x'").unwrap();
    e.execute("ALTER VIEW rv OWNER TO r9").unwrap();
    e.execute("ALTER VIEW rv RENAME TO rv2").unwrap();
    assert_eq!(
        col(
            &mut e,
            "SELECT viewowner FROM pg_views WHERE viewname='rv2'"
        ),
        vec!["r9".to_string()]
    );
}

#[test]
fn a_missing_name_and_a_taken_one_are_refused_as_pg_refuses_them() {
    let mut e = seeded();
    for (sql, want) in [
        (
            "ALTER VIEW nov RENAME TO x",
            "relation \"nov\" does not exist",
        ),
        ("ALTER TYPE noc RENAME TO x", "type \"noc\" does not exist"),
        (
            "ALTER VIEW rv RENAME TO rs",
            "relation \"rs\" already exists",
        ),
        ("ALTER TYPE rt RENAME TO rt", "type \"rt\" already exists"),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "{sql}\n  got: {got}\n  want: {want}");
        // 9.0.0 — the first cut raised these through `StorageError::
        // Corrupt`, which reads "corrupt on-disk format: …" to a client.
        assert!(!got.contains("corrupt on-disk format"), "{sql}: {got}");
    }
}
