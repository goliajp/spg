//! Round 675 — the catalogs said no column had a collation.
//!
//! `pg_attribute.attcollation` was hard-coded 0 at both fill sites, under a
//! comment reading "0 (default)". 0 is not the default — it is what PG puts
//! on a type that has no collation at all. And
//! `information_schema.columns.collation_name` did not exist, so asking for
//! it was an error rather than an answer.
//!
//! SPG's `pg_collation` already listed the three collations it can perform
//! with PG's own oids — default 100, C 950, POSIX 951. What was missing was
//! the wire between a catalog that was already right and the columns
//! reporting nothing.
//!
//! Still true and recorded rather than fixed: a column written with an
//! explicit `COLLATE` reports its TYPE's collation here, because the name is
//! discarded during parsing and there is nowhere to read it back from. That
//! is F36's "the declaration is taken and ignored", and the next step in
//! `docs/COLLATION_RFC.md` §5.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> String {
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

fn seed(e: &mut Engine) {
    e.execute(
        "CREATE TABLE ac(t TEXT, v VARCHAR(8), c CHAR(4), i INT, d DATE, \
         b BYTEA, u UUID, ip INET, num NUMERIC, ts TIMESTAMP, bo BOOLEAN)",
    )
    .unwrap();
}

/// PG18-verified, column by column: the three collatable types carry 100,
/// everything else carries 0.
#[test]
fn round675_attcollation_names_the_default_collation() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(
        rows(
            &mut e,
            "SELECT attname, attcollation FROM pg_attribute \
             WHERE attrelid = 'ac'::regclass AND attnum > 0 ORDER BY attnum"
        ),
        "t|100,v|100,c|100,i|0,d|0,b|0,u|0,ip|0,num|0,ts|0,bo|0"
    );
}

/// The oids have to be the ones `pg_collation` actually publishes, or a
/// client joining the two gets nothing.
#[test]
fn round675_the_oids_join_pg_collation() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(
        rows(
            &mut e,
            "SELECT collname FROM pg_collation WHERE oid = 100"
        ),
        "default"
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT a.attname, c.collname FROM pg_attribute a \
             JOIN pg_collation c ON c.oid = a.attcollation \
             WHERE a.attrelid = 'ac'::regclass AND a.attnum > 0 ORDER BY a.attnum"
        ),
        "t|default,v|default,c|default"
    );
}

/// `collation_name` exists now, and names the collation rather than its oid.
#[test]
fn round675_information_schema_reports_a_collation_name() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(
        rows(
            &mut e,
            "SELECT column_name, collation_name FROM information_schema.columns \
             WHERE table_name = 'ac' AND column_name IN ('t','i') ORDER BY ordinal_position"
        ),
        "t|default,i|NULL"
    );
}
