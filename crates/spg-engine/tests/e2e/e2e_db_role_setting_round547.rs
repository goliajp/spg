//! v7.39 (round 547) — `ALTER ROLE … SET` did nothing, and said it worked.
//!
//! Round 546 put `pg_db_role_setting` in the empty family on the
//! grounds that SPG recorded no per-role settings. It did not record
//! them because the statement fell into the parser's pg_dump no-op
//! tail: `ALTER ROLE r SET work_mem = '8MB'` answered ALTER ROLE and
//! changed nothing. A DBA setting a per-role default got no effect and
//! no error — the shape this campaign puts above missing features.
//!
//! PG's model, measured on PG18 and reproduced here:
//!
//!     ALTER ROLE r SET p = v                 (0,   role)
//!     ALTER DATABASE d SET p = v             (db,  0)
//!     ALTER ROLE r IN DATABASE d SET p = v   (db,  role)
//!     ALTER ROLE ALL SET p = v               (0,   0)
//!
//! `RESET p` drops that one parameter; `RESET ALL` drops the whole
//! entry for THAT scope and leaves the other three standing. A new
//! session applies them least-specific first, so the most specific
//! wins — measured: with all four set, a fresh connection reads the
//! role-in-database value.
//!
//! `ALL` lexes as a KEYWORD, not an identifier, so the ordinary name
//! reader refused `ALTER ROLE ALL` — the same trap as TABLE, INDEX,
//! FULL and DEFAULT before it, for the sixth time.
//!
//! FILE_VERSION 84 → 85; the settings survive a snapshot round-trip.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
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
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// Every scope, keyed as PG keys it.
#[test]
fn round547_the_four_scopes() {
    let mut e = Engine::new();
    e.execute("ALTER ROLE postgres SET work_mem = '8MB'")
        .unwrap();
    e.execute("ALTER DATABASE spg SET work_mem = '7MB'")
        .unwrap();
    e.execute("ALTER ROLE postgres IN DATABASE spg SET work_mem = '6MB'")
        .unwrap();
    e.execute("ALTER ROLE ALL SET work_mem = '9MB'").unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT setdatabase, setrole, setconfig FROM pg_db_role_setting ORDER BY 1, 2"
        ),
        vec![
            "0|0|{work_mem=9MB}",
            "0|10|{work_mem=8MB}",
            "16384|0|{work_mem=7MB}",
            "16384|10|{work_mem=6MB}",
        ]
    );
}

/// RESET drops one parameter; RESET ALL drops only that scope.
#[test]
fn round547_reset_semantics() {
    let mut e = Engine::new();
    e.execute("ALTER ROLE postgres SET work_mem = '8MB'")
        .unwrap();
    e.execute("ALTER ROLE postgres SET statement_timeout = '5s'")
        .unwrap();
    e.execute("ALTER ROLE postgres RESET work_mem").unwrap();
    assert_eq!(
        rows(&mut e, "SELECT setconfig FROM pg_db_role_setting"),
        vec!["{statement_timeout=5s}"]
    );
    // The other scopes survive a RESET ALL of this one.
    e.execute("ALTER DATABASE spg SET work_mem = '7MB'")
        .unwrap();
    e.execute("ALTER ROLE ALL SET work_mem = '9MB'").unwrap();
    e.execute("ALTER ROLE postgres RESET ALL").unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT setdatabase, setrole, setconfig FROM pg_db_role_setting ORDER BY 1, 2"
        ),
        vec!["0|0|{work_mem=9MB}", "16384|0|{work_mem=7MB}"]
    );
    // A scope with nothing left in it leaves no row.
    e.execute("ALTER ROLE ALL RESET work_mem").unwrap();
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM pg_db_role_setting"),
        vec!["1"]
    );
}

/// A scope naming something that is not there is refused, as in PG.
#[test]
fn round547_unknown_scope_is_refused() {
    let mut e = Engine::new();
    for (sql, needle) in [
        ("ALTER ROLE nosuchrole SET work_mem = '1MB'", "nosuchrole"),
        ("ALTER DATABASE nosuchdb SET work_mem = '1MB'", "nosuchdb"),
    ] {
        let err = format!("{}", e.execute(sql).expect_err(sql));
        assert!(err.contains(needle), "{sql}: message was {err}");
        assert!(err.contains("does not exist"), "{sql}: message was {err}");
    }
}

/// `SET p TO v` is PG's other spelling, and a bare value needs no quotes.
#[test]
fn round547_both_spellings() {
    let mut e = Engine::new();
    e.execute("ALTER ROLE postgres SET search_path TO public")
        .unwrap();
    e.execute("ALTER ROLE ALL SET statement_timeout = 30000")
        .unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT setdatabase, setrole, setconfig FROM pg_db_role_setting ORDER BY 1, 2"
        ),
        vec!["0|0|{statement_timeout=30000}", "0|10|{search_path=public}"]
    );
}

/// The settings survive a snapshot round-trip — what FILE_VERSION 85 is for.
#[test]
fn round547_settings_survive_a_reload() {
    let mut e = Engine::new();
    e.execute("ALTER ROLE postgres SET work_mem = '8MB'")
        .unwrap();
    e.execute("ALTER ROLE ALL SET statement_timeout = '5s'")
        .unwrap();
    let snapshot = e.catalog().serialize();
    let mut back =
        Engine::restore(spg_storage::Catalog::deserialize(&snapshot).expect("roundtrip"));
    assert_eq!(
        rows(
            &mut back,
            "SELECT setdatabase, setrole, setconfig FROM pg_db_role_setting ORDER BY 1, 2"
        ),
        vec!["0|0|{statement_timeout=5s}", "0|10|{work_mem=8MB}"]
    );
}

/// A plain ALTER ROLE without SET keeps its old path.
#[test]
fn round547_plain_alter_role_unaffected() {
    let mut e = Engine::new();
    // These are accepted-and-ignored so a pg_dump role block restores;
    // the new interception must not have taken them over.
    for sql in [
        "ALTER ROLE postgres WITH CREATEDB",
        "ALTER ROLE postgres NOSUPERUSER",
    ] {
        e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
    }
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM pg_db_role_setting"),
        vec!["0"]
    );
}
