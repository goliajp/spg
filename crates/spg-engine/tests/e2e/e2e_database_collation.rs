//! v7.38.18 — the database collation: S1, S2 and S3 of
//! `docs/DESIGN-2026-08-23-collation.md`.
//!
//! Every expectation is PostgreSQL 18.4's, taken on a database that
//! collates as `en_US.utf8` — the stock Debian default and the thing
//! SPG is dropped in for.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
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
            .collect::<Vec<_>>()
            .join(" "),
        other => format!("{other:?}"),
    }
}

fn seeded(collation: Option<&str>) -> Engine {
    let mut e = Engine::new();
    if let Some(c) = collation {
        e.set_database_collation(c)
            .expect("a fresh database takes one");
    }
    e.execute("CREATE TABLE t(x TEXT, y TEXT COLLATE \"C\")")
        .unwrap();
    e.execute(
        "INSERT INTO t VALUES ('Zebra','Zebra'),('apple','apple'),('Bob','Bob'),('client','client')",
    )
    .unwrap();
    e
}

/// The negative control, and the one that matters most: a database that
/// never asked for a collation answers exactly as it always did.
///
/// Every other test in this workspace runs against such a database, so
/// the 8,500 of them are this claim's real evidence; this states it.
#[test]
fn a_c_database_is_byte_ordered_exactly_as_before() {
    let mut e = seeded(None);
    assert_eq!(
        one(&mut e, "SELECT DISTINCT datcollate FROM pg_database"),
        "C"
    );
    assert_eq!(
        one(&mut e, "SELECT string_agg(x, ' ' ORDER BY x) FROM t"),
        "Bob Zebra apple client"
    );
    assert_eq!(one(&mut e, "SELECT max(x) FROM t"), "client");
    assert_eq!(
        one(&mut e, "SELECT x FROM t WHERE x < 'b'"),
        "Zebra apple Bob"
    );
    assert_eq!(one(&mut e, "SELECT ('B' < 'a')"), "Bool(true)");
}

/// An undeclared text column inherits the database's collation, at every
/// surface that orders text. PG 18.4's answers, on its own en_US.utf8
/// database, are on the right.
#[test]
fn an_undeclared_column_inherits_the_database_collation() {
    let mut e = seeded(Some("en_US.utf8"));
    assert_eq!(
        one(&mut e, "SELECT DISTINCT datcollate FROM pg_database"),
        "en_US.utf8"
    );
    // ORDER BY, through the statement's own sort...
    assert_eq!(
        one(&mut e, "SELECT x FROM t ORDER BY x"),
        "apple Bob client Zebra"
    );
    // ...through an aggregate's, which is a separate comparator...
    assert_eq!(
        one(&mut e, "SELECT string_agg(x, ' ' ORDER BY x) FROM t"),
        "apple Bob client Zebra"
    );
    // ...and through min/max, which is a third.
    assert_eq!(one(&mut e, "SELECT max(x) FROM t"), "Zebra");
    // Range comparisons in a scan filter, which is a fourth and was the
    // last to be wired: it answered by bytes while ORDER BY answered by
    // the locale, inside one query.
    assert_eq!(one(&mut e, "SELECT x FROM t WHERE x < 'b'"), "apple");
    assert_eq!(
        one(&mut e, "SELECT x FROM t WHERE x > 'b' ORDER BY x"),
        "Bob client Zebra"
    );
    // And a comparison of two literals, which has no column at all.
    assert_eq!(one(&mut e, "SELECT ('B' < 'a')"), "Bool(false)");
}

/// Inheritance reaches ORDERING and not PADDING, and it does not reach
/// a column that declared something of its own.
///
/// `pads` is a MySQL property of a MySQL collation name. PostgreSQL does
/// not pad — `'a' = 'a  '` is false on that oracle — and an earlier
/// draft fed the inherited name to `pads_space`, which would have made
/// it true for every text column in the database.
#[test]
fn inheritance_reaches_ordering_and_not_padding() {
    let mut e = seeded(Some("en_US.utf8"));
    assert_eq!(one(&mut e, "SELECT ('a' = 'a  ')"), "Bool(false)");
    // A column that declares `C` keeps byte order beside one that does
    // not declare anything.
    assert_eq!(
        one(&mut e, "SELECT string_agg(y, ' ' ORDER BY y) FROM t"),
        "Bob Zebra apple client"
    );
    // ...and the catalog still reports NULL for the inheriting column,
    // which is what PG reports: it inherits, it does not carry.
    assert_eq!(
        one(
            &mut e,
            "SELECT collation_name FROM information_schema.columns \
             WHERE table_name = 't' AND column_name = 'x'"
        ),
        "<NULL>"
    );
}

/// An index on an inheriting column agrees with the scan (S0 x S2).
///
/// This is why S0 had to land first: inheriting a locale collation
/// before index keys carried one would have spread the missing-rows
/// defect from the few columns that declare a collation to every text
/// column in the database.
#[test]
fn an_index_on_an_inheriting_column_agrees_with_the_scan() {
    let mut e = seeded(Some("en_US.utf8"));
    let before = one(&mut e, "SELECT x FROM t WHERE x > 'b' ORDER BY x");
    e.execute("CREATE INDEX ix ON t(x)").unwrap();
    assert_eq!(
        one(&mut e, "SELECT x FROM t WHERE x > 'b' ORDER BY x"),
        before,
        "an index must not change the answer"
    );
    assert_eq!(one(&mut e, "SELECT x FROM t WHERE x = 'apple'"), "apple");
    e.execute("INSERT INTO t VALUES ('Charlie','Charlie')")
        .unwrap();
    assert_eq!(
        one(&mut e, "SELECT x FROM t WHERE x > 'b' ORDER BY x"),
        "Bob Charlie client Zebra"
    );
}

/// A COMPOSITE index over an inheriting text column must not change the
/// answer either.
///
/// Its entries are tuples of raw cells, built by storage, while
/// `probe_key` encodes a locale-collated column's probe as an ICU sort
/// key — two different spaces, and the seek finds nothing in the wrong
/// one. The differential corpus caught this as `WHERE id = 7 AND s =
/// 'row7'` answering 0 where PostgreSQL 18.4 answers 1.
///
/// Sixty rows, because the composite seek has a cost cap and a tiny
/// table scans regardless: a fixture built on four rows passes with the
/// defect present.
#[test]
fn a_composite_index_over_an_inheriting_column_agrees_with_the_scan() {
    let mut e = Engine::new();
    e.set_database_collation("en_US.utf8").unwrap();
    e.execute("CREATE TABLE ci(id INT, s TEXT)").unwrap();
    for i in 1..=60 {
        e.execute(&format!("INSERT INTO ci VALUES ({i}, 'row{i}')"))
            .unwrap();
    }
    let before = one(
        &mut e,
        "SELECT count(*) FROM ci WHERE id = 7 AND s = 'row7'",
    );
    assert_eq!(before, "BigInt(1)");
    e.execute("CREATE INDEX ci_multi ON ci(id, s)").unwrap();
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM ci WHERE id = 7 AND s = 'row7'"
        ),
        before,
        "the composite index must not change the answer"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM ci WHERE id = 40 AND s = 'row40'"
        ),
        "BigInt(1)"
    );
    // ...and a predicate that matches nothing still matches nothing. A
    // seek that narrowed by `id` alone and forgot to re-check `s` would
    // answer 1 here.
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM ci WHERE id = 7 AND s = 'nope'"
        ),
        "BigInt(0)"
    );
    // v7.38.18 (G3) — and it SEEKS rather than scanning. The first fix
    // for the wrong-space probe declined such an index outright, which
    // cost a full scan for a predicate `id` alone narrows to one row.
    // The prefix stops at the collated component instead, and the caller
    // re-checks the rest.
    let before = one(
        &mut e,
        "SELECT idx_scan FROM pg_stat_user_tables WHERE relname = 'ci'",
    );
    let _ = one(
        &mut e,
        "SELECT count(*) FROM ci WHERE id = 7 AND s = 'row7'",
    );
    let after = one(
        &mut e,
        "SELECT idx_scan FROM pg_stat_user_tables WHERE relname = 'ci'",
    );
    assert_ne!(
        before, after,
        "the composite index must be read, not skipped"
    );
}

/// Set once. PostgreSQL refuses `ALTER DATABASE … LC_COLLATE` and so
/// does this, because every index key here was built under what it has.
#[test]
fn the_database_collation_is_set_once() {
    let mut e = seeded(Some("en_US.utf8"));
    assert!(
        e.set_database_collation("de_DE.utf8").is_err(),
        "changing it would leave every index key ordered by the old one"
    );
    // Asking for what it already has is not an error -- a host that
    // passes its environment on every start must not fail on restart.
    assert_eq!(e.set_database_collation("en_US.utf8").ok(), Some(false));
    assert_eq!(
        one(&mut e, "SELECT DISTINCT datcollate FROM pg_database"),
        "en_US.utf8"
    );
}

/// A collation this build cannot perform is refused at the door (S3).
/// Recording it would mean a database whose comparator does not exist.
///
/// The refusal is narrower than it first looks, and the narrowness is
/// worth stating rather than discovering. ICU falls back to the root
/// collation for a well-formed language tag it has no data for, so
/// `kl_KL.no_such_locale` and `zz_ZZ` are ACCEPTED — this build really
/// can perform them, as root. What it rejects is a name that is not a
/// language tag at all. PostgreSQL, which validates against its own
/// catalogue, answers `collation "x" for encoding "UTF8" does not
/// exist` for both kinds; SPG cannot tell them apart, and says so in
/// `docs/FINDING-2026-08-23-database-collation.md`.
#[test]
fn an_unperformable_collation_is_refused() {
    let mut e = Engine::new();
    for bad in ["not a locale!", "??", "", "xxxxxxxxxxxx"] {
        assert!(
            e.set_database_collation(bad).is_err(),
            "{bad:?} is not a language tag and must not be recorded"
        );
        assert_eq!(e.database_collation(), "C", "and nothing is recorded");
    }
    // v7.38.18 (G2) — and a well-formed tag PostgreSQL does not have is
    // refused too, now that there is a catalogue to check against.
    //
    // This asserted the opposite when it was written, with a comment
    // explaining that ICU falls back to the root collation for any
    // language tag and that recording `zz_ZZ` was therefore honest
    // because "the comparator exists". The comparator does exist; the
    // COLLATION does not, and PostgreSQL 18.4 answers `collation
    // "zz_ZZ" for encoding "UTF8" does not exist`. So does this now.
    let err = format!("{:?}", e.set_database_collation("zz_ZZ").unwrap_err());
    assert!(err.contains("does not exist"), "{err}");
    assert_eq!(e.database_collation(), "C");
}

/// It survives a save/restore, and so do the answers.
#[test]
fn the_collation_survives_a_round_trip() {
    let mut e = seeded(Some("en_US.utf8"));
    e.execute("CREATE INDEX ix ON t(x)").unwrap();
    let bytes = e.snapshot();
    let mut back = Engine::restore_envelope(&bytes).expect("restores");
    assert_eq!(back.database_collation(), "en_US.utf8");
    assert_eq!(
        one(&mut back, "SELECT x FROM t ORDER BY x"),
        "apple Bob client Zebra"
    );
    // The index came back too, and still agrees.
    assert_eq!(
        one(&mut back, "SELECT x FROM t WHERE x > 'b' ORDER BY x"),
        "Bob client Zebra"
    );
}

/// `CREATE DATABASE … LC_COLLATE` — the SQL path.
///
/// v7.38.18. The design note, the CHANGELOG and a customer letter all
/// said a database could be created with a collation, and every test
/// behind that claim went through `set_database_collation`, the Rust
/// API. The statement itself was parsed and thrown away whole: SPG is
/// single-database, so `CREATE DATABASE` had been a no-op since v7.14,
/// and the `LC_COLLATE` on it went with the rest of the tokens.
///
/// It is in every PostgreSQL bootstrap script there is. A database that
/// sorts by the container's `LANG` instead of the one the script asked
/// for gives a different answer to every `ORDER BY` it will ever run,
/// and says nothing.
mod create_database_lc_collate {
    use super::{Engine, one};

    #[test]
    fn the_statement_sets_the_collation() {
        let mut e = Engine::new();
        e.execute("CREATE DATABASE app LC_COLLATE 'de_DE.utf8'")
            .unwrap();
        assert_eq!(
            one(&mut e, "SELECT DISTINCT datcollate FROM pg_database"),
            "de_DE.utf8"
        );
        // And it reaches ordering, which is the only reason to want it.
        e.execute("CREATE TABLE t(x TEXT)").unwrap();
        e.execute("INSERT INTO t VALUES ('Zebra'),('apfel'),('Bob')")
            .unwrap();
        assert_eq!(
            one(&mut e, "SELECT string_agg(x, ',' ORDER BY x) FROM t"),
            "apfel,Bob,Zebra"
        );
    }

    /// `LOCALE` is the other spelling, and `=` is optional in both.
    #[test]
    fn locale_and_an_equals_sign_are_the_same_statement() {
        for sql in [
            "CREATE DATABASE app LOCALE 'de_DE.utf8'",
            "CREATE DATABASE app LC_COLLATE = 'de_DE.utf8'",
            "CREATE DATABASE app WITH TEMPLATE = template0 LC_COLLATE = 'de_DE.utf8' LC_CTYPE = 'de_DE.utf8'",
        ] {
            let mut e = Engine::new();
            e.execute(sql).unwrap();
            assert_eq!(
                one(&mut e, "SELECT DISTINCT datcollate FROM pg_database"),
                "de_DE.utf8",
                "{sql}"
            );
        }
    }

    /// The negative control the whole feature rests on: a plain
    /// `CREATE DATABASE` is still a no-op, and still leaves `C`.
    #[test]
    fn a_statement_without_a_collation_changes_nothing() {
        let mut e = Engine::new();
        e.execute("CREATE DATABASE app").unwrap();
        assert_eq!(
            one(&mut e, "SELECT DISTINCT datcollate FROM pg_database"),
            "C"
        );
        assert_eq!(one(&mut e, "SELECT ('B' < 'a')"), "Bool(true)");
    }

    /// Once a table exists its index keys were built under the old
    /// collation, so the statement cannot take effect. It must not fail
    /// either: PostgreSQL would have created a SEPARATE database here
    /// and returned success, and failing a bootstrap script is a
    /// customer change.
    #[test]
    fn a_database_with_tables_warns_and_succeeds() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE t(x TEXT)").unwrap();
        e.execute("CREATE DATABASE app LC_COLLATE 'de_DE.utf8'")
            .expect("must not fail a bootstrap script");
        assert_eq!(
            one(&mut e, "SELECT DISTINCT datcollate FROM pg_database"),
            "C"
        );
    }

    /// v7.38.19 — the created name reaches `pg_database`.
    ///
    /// sentori reported this against 7.38.18: `CREATE DATABASE dd`
    /// succeeded, `dd` connected and answered `current_database()`, and
    /// the catalogue listed one row — whichever name the asking session
    /// had used. `psql \l`, a migration tool asking "does this database
    /// exist", and a backup script that enumerates all read that table.
    #[test]
    fn a_created_database_is_listed_in_the_catalogue() {
        let mut e = Engine::new();
        e.execute("CREATE DATABASE dd LC_COLLATE 'de_DE.utf8'")
            .unwrap();
        let listed = one(
            &mut e,
            "SELECT string_agg(datname, ',' ORDER BY datname) FROM pg_database",
        );
        assert!(
            listed.contains("dd"),
            "pg_database must list the database just created: {listed}"
        );
        // The names are aliases onto one database, so both rows carry the
        // same collation. Saying otherwise would be a second lie.
        assert_eq!(
            one(&mut e, "SELECT count(DISTINCT datcollate) FROM pg_database"),
            "BigInt(1)"
        );
    }

    /// A `CREATE DATABASE` with no collation still records its name: the
    /// catalogue question is independent of the collation one.
    #[test]
    fn the_name_is_recorded_even_without_a_collation() {
        let mut e = Engine::new();
        e.execute("CREATE DATABASE plain").unwrap();
        let listed = one(
            &mut e,
            "SELECT string_agg(datname, ',' ORDER BY datname) FROM pg_database",
        );
        assert!(listed.contains("plain"), "{listed}");
        // And nothing about the collation moved.
        assert_eq!(
            one(&mut e, "SELECT DISTINCT datcollate FROM pg_database"),
            "C"
        );
    }

    /// The name is the database's, not the keyword's. It was the
    /// keyword: `scan_database_name` was called with the parser still
    /// sitting on `DATABASE`, and `pg_database` listed a database called
    /// `database`.
    #[test]
    fn the_recorded_name_is_not_the_keyword() {
        let mut e = Engine::new();
        e.execute("CREATE DATABASE dd").unwrap();
        let listed = one(
            &mut e,
            "SELECT string_agg(datname, ',' ORDER BY datname) FROM pg_database",
        );
        assert!(!listed.contains("database"), "{listed}");
        assert!(listed.contains("dd"), "{listed}");
        // A quoted name keeps its spelling.
        e.execute("CREATE DATABASE \"Mixed\"").unwrap();
        let listed = one(
            &mut e,
            "SELECT string_agg(datname, ',' ORDER BY datname) FROM pg_database",
        );
        assert!(listed.contains("Mixed"), "{listed}");

        // Not tested here: `CREATE DATABASE IF NOT EXISTS`. PostgreSQL 18
        // answers it `syntax error at or near \"NOT\"` -- it is MySQL's
        // spelling -- so asserting SPG accepts it would be asserting a
        // behaviour the engine we stand in for does not have. The
        // keyword-skipping in `scan_database_name` stays for the MySQL
        // dialect's sake and is covered by the name above.
    }

    /// A name PostgreSQL does not have is refused here too, rather than
    /// letting ICU fall back to the root locale.
    #[test]
    fn an_unknown_collation_is_refused() {
        let mut e = Engine::new();
        assert!(
            e.execute("CREATE DATABASE app LC_COLLATE 'zz_ZZ.utf8'")
                .is_err()
        );
    }
}
