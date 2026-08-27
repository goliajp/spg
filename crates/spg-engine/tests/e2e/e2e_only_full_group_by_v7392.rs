//! v7.39.2 — `ONLY_FULL_GROUP_BY` is MySQL's sixth default, and it is
//! enforced here now.
//!
//! `SELECT g, v FROM t GROUP BY g` in a MySQL session answered an
//! arbitrary row's `v` per group. MySQL 9.7.2 answers `ERROR 1055`,
//! because its DEFAULT `sql_mode` carries the flag — SPG's dialect
//! ignored `sql_mode` entirely and was loose whatever it said.
//!
//! The rule was already in the tree: it is PostgreSQL's, enforced on
//! the PG side, and switched off wholesale for the dialect on a flag
//! that means "this session speaks MySQL". It asks `sql_mode` now.
//!
//! Four other checks in `aggregate.rs` read that same flag and ask
//! DIFFERENT questions through it — MySQL's column naming, HAVING
//! aliases, collation folding — and are deliberately untouched. A grep
//! and replace would have changed three unrelated behaviours.

use spg_engine::{Engine, IMPLICIT_TX, QueryResult, TxId};

fn ask(e: &mut Engine, session: u32, tx: TxId, sql: &str) -> Result<String, String> {
    e.set_current_session(session);
    match e.execute_in(sql, tx) {
        Err(err) => Err(format!("{err}")),
        Ok(QueryResult::Rows { rows, .. }) if !rows.is_empty() => Ok(rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .collect::<Vec<_>>()
            .join("|")),
        Ok(_) => Ok("<none>".to_string()),
    }
}

fn seeded(e: &mut Engine) {
    ask(
        e,
        1,
        IMPLICIT_TX,
        "CREATE TABLE t (id int PRIMARY KEY, g int, v int)",
    )
    .unwrap();
    ask(
        e,
        1,
        IMPLICIT_TX,
        "INSERT INTO t VALUES (1,1,10),(2,1,20),(3,2,30)",
    )
    .unwrap();
}

const REFUSAL: &str = "must appear in the GROUP BY clause";

#[test]
fn a_mysql_session_refuses_a_bare_non_aggregated_column() {
    let mut e = Engine::new();
    seeded(&mut e);
    ask(&mut e, 1, IMPLICIT_TX, "SET sql_mode=''").unwrap();
    // Loose, because this list does not carry the flag — MySQL's answer
    // too.
    assert!(ask(&mut e, 1, IMPLICIT_TX, "SELECT g, v FROM t GROUP BY g").is_ok());

    // The default list DOES carry it.
    ask(&mut e, 1, IMPLICIT_TX, "SET sql_mode='ONLY_FULL_GROUP_BY'").unwrap();
    for sql in [
        "SELECT g, v FROM t GROUP BY g",
        "SELECT g FROM t GROUP BY g HAVING v > 1",
        "SELECT g FROM t GROUP BY g ORDER BY v",
    ] {
        assert!(
            ask(&mut e, 1, IMPLICIT_TX, sql)
                .unwrap_err()
                .contains(REFUSAL),
            "{sql} must be refused under ONLY_FULL_GROUP_BY"
        );
    }
}

#[test]
fn what_the_flag_must_not_refuse() {
    // Refusing too much is the failure this class invites, and MySQL
    // agrees with PostgreSQL on all of these.
    let mut e = Engine::new();
    seeded(&mut e);
    ask(&mut e, 1, IMPLICIT_TX, "SET sql_mode='ONLY_FULL_GROUP_BY'").unwrap();
    assert_eq!(
        ask(
            &mut e,
            1,
            IMPLICIT_TX,
            "SELECT g, sum(v) FROM t GROUP BY g ORDER BY g"
        )
        .unwrap(),
        "1,30|2,30",
        "an aggregate over the ungrouped column is the point of GROUP BY"
    );
    assert_eq!(
        ask(
            &mut e,
            1,
            IMPLICIT_TX,
            "SELECT g FROM t GROUP BY g ORDER BY g"
        )
        .unwrap(),
        "1|2"
    );
    assert_eq!(
        ask(
            &mut e,
            1,
            IMPLICIT_TX,
            "SELECT id, v FROM t GROUP BY id ORDER BY id"
        )
        .unwrap(),
        "1,10|2,20|3,30",
        "grouping by the primary key determines every other column — \
         functional dependency, which both engines allow"
    );
}

#[test]
fn a_postgresql_session_was_already_strict_and_still_is() {
    let mut e = Engine::new();
    seeded(&mut e);
    assert!(
        ask(&mut e, 1, IMPLICIT_TX, "SELECT g, v FROM t GROUP BY g")
            .unwrap_err()
            .contains(REFUSAL)
    );
    // And its functional dependency is unchanged.
    assert_eq!(
        ask(
            &mut e,
            1,
            IMPLICIT_TX,
            "SELECT id, v FROM t GROUP BY id ORDER BY id"
        )
        .unwrap(),
        "1,10|2,20|3,30"
    );
}

#[test]
fn the_default_sql_mode_claims_it_because_it_keeps_it() {
    let mut e = Engine::new();
    assert!(
        spg_engine::MYSQL_DEFAULT_SQL_MODE.contains("ONLY_FULL_GROUP_BY"),
        "claimed"
    );
    seeded(&mut e);
    // A session that never sets sql_mode has MySQL's default, so a
    // fresh MySQL connection is strict.
    ask(&mut e, 1, IMPLICIT_TX, "SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    assert!(
        ask(&mut e, 1, IMPLICIT_TX, "SELECT g, v FROM t GROUP BY g").is_ok(),
        "a list without the flag is loose, which is MySQL's answer too"
    );
}

#[test]
fn the_setting_is_per_connection() {
    // Two sessions, opposite sql_mode. The shared engine must not let
    // one decide for the other.
    let mut e = Engine::new();
    let a = e.alloc_tx_id();
    let b = e.alloc_tx_id();
    ask(
        &mut e,
        1,
        a,
        "CREATE TABLE t (id int PRIMARY KEY, g int, v int)",
    )
    .unwrap();
    ask(&mut e, 1, a, "INSERT INTO t VALUES (1,1,10),(2,1,20)").unwrap();
    ask(&mut e, 1, a, "SET sql_mode=''").unwrap();
    ask(&mut e, 2, b, "SET sql_mode='ONLY_FULL_GROUP_BY'").unwrap();
    assert!(
        ask(&mut e, 1, a, "SELECT g, v FROM t GROUP BY g").is_ok(),
        "A said no flag"
    );
    assert!(
        ask(&mut e, 2, b, "SELECT g, v FROM t GROUP BY g")
            .unwrap_err()
            .contains(REFUSAL),
        "B said flag"
    );
}
