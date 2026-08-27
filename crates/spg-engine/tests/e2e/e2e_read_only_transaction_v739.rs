//! v7.39 — read-only transactions, which SPG did not enforce at all.
//!
//! `BEGIN READ ONLY; INSERT …` answered `INSERT 0 1` and committed.
//! `SET default_transaction_read_only = on` changed nothing either,
//! though both GUCs were in the inventory and both read back whatever
//! they were given — so a session could ask for the mode, be told it had
//! it, and write anyway.
//!
//! Applications open read-only transactions as a SAFETY measure: a
//! reporting connection, a read-only leg in a pool, a "this code path
//! must not write" discipline. Accepting the writes is the worst
//! available answer, because the whole point of asking was to be stopped.
//!
//! Every expectation here was read back from PostgreSQL 18.6 by running
//! the statement inside `BEGIN READ ONLY`, one statement per transaction
//! so no error could be attributed to a neighbouring line. Several were
//! not what one would guess, which is why they were measured:
//! `CREATE TEMP TABLE` is refused, `NOTIFY` and `REINDEX` are allowed,
//! `PREPARE` of an INSERT is allowed, and `UPDATE … WHERE false` — which
//! changes nothing — is still refused.

use spg_engine::{Engine, EngineError, QueryResult};
use spg_storage::Value;

fn setup() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ro (x INT)").unwrap();
    e.execute("INSERT INTO ro VALUES (1)").unwrap();
    e.execute("CREATE SEQUENCE sq").unwrap();
    e
}

fn row_text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        Value::Text(t) => t.to_string(),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default(),
        other => panic!("{sql}: expected rows, got {other:?}"),
    }
}

/// The tag in the message is PG's command tag for the statement, so a
/// client that greps for one gets the same string it gets from PG.
#[test]
fn a_read_only_transaction_refuses_every_write_pg_refuses() {
    for (sql, tag) in [
        ("INSERT INTO ro VALUES (2)", "INSERT"),
        ("UPDATE ro SET x = 9", "UPDATE"),
        // Changes no rows, and is refused anyway: the verb decides.
        ("UPDATE ro SET x = x WHERE false", "UPDATE"),
        ("DELETE FROM ro", "DELETE"),
        ("TRUNCATE ro", "TRUNCATE TABLE"),
        ("CREATE TABLE t2 (a INT)", "CREATE TABLE"),
        // PG refuses this too, tagged CREATE TABLE, despite the general
        // belief that temporary tables are exempt.
        ("CREATE TEMP TABLE tt (a INT)", "CREATE TABLE"),
        ("DROP TABLE ro", "DROP TABLE"),
        ("CREATE INDEX ix ON ro (x)", "CREATE INDEX"),
        ("CREATE VIEW vv AS SELECT 1", "CREATE VIEW"),
        ("CREATE SEQUENCE sq2", "CREATE SEQUENCE"),
        ("COMMENT ON TABLE ro IS 'x'", "COMMENT"),
        ("GRANT SELECT ON ro TO admin", "GRANT"),
        ("SELECT x FROM ro FOR UPDATE", "SELECT FOR UPDATE"),
        ("SELECT x FROM ro FOR SHARE", "SELECT FOR SHARE"),
        // Reached inside a plain SELECT, so the statement-level check
        // cannot see it. PG names the function, not the statement.
        ("SELECT nextval('sq')", "nextval()"),
        ("SELECT setval('sq', 5)", "setval()"),
    ] {
        let mut e = setup();
        e.execute("BEGIN READ ONLY").unwrap();
        let err = e.execute(sql).expect_err(&format!(
            "`{sql}` must be refused in a read-only transaction"
        ));
        let want = format!("cannot execute {tag} in a read-only transaction");
        assert!(
            err.to_string().contains(&want),
            "`{sql}`: wanted {want:?}, got {err}"
        );
        assert!(matches!(err, EngineError::Unsupported(_)));
    }
}

/// The other half: a read-only transaction is still a transaction. PG
/// allows all of these, and refusing them would be its own defect.
#[test]
fn a_read_only_transaction_still_allows_what_pg_allows() {
    for sql in [
        "SELECT 1",
        "SELECT count(*) FROM ro",
        "ANALYZE ro",
        "SET LOCAL work_mem = 4096",
        "SAVEPOINT sp",
        "NOTIFY chan",
        "LISTEN chan",
        "DECLARE c CURSOR FOR SELECT 1",
        // The write is in the EXECUTE, not the PREPARE.
        "PREPARE p AS INSERT INTO ro VALUES (1)",
        // Reads the session's last value; moves nothing.
        "SELECT currval('sq')",
    ] {
        let mut e = setup();
        // `currval` needs a `nextval` first, and that one has to happen
        // outside the read-only block.
        e.execute("SELECT nextval('sq')").unwrap();
        e.execute("BEGIN READ ONLY").unwrap();
        e.execute(sql)
            .unwrap_or_else(|err| panic!("`{sql}` is allowed by PG 18.6 but was refused: {err}"));
    }
}

/// Measured on PG 18.6: `off`, `on` inside the block, `off` after, `on`
/// once the setting is on — inside a transaction and outside one.
#[test]
fn the_read_only_mode_is_reported_the_way_pg_reports_it() {
    let mut e = setup();
    assert_eq!(row_text(&mut e, "SHOW transaction_read_only"), "off");

    e.execute("BEGIN READ ONLY").unwrap();
    assert_eq!(row_text(&mut e, "SHOW transaction_read_only"), "on");
    e.execute("COMMIT").unwrap();
    assert_eq!(row_text(&mut e, "SHOW transaction_read_only"), "off");

    e.execute("SET default_transaction_read_only = on").unwrap();
    assert_eq!(
        row_text(&mut e, "SHOW transaction_read_only"),
        "on",
        "outside a transaction PG reports the session default"
    );
    e.execute("BEGIN").unwrap();
    assert_eq!(row_text(&mut e, "SHOW transaction_read_only"), "on");
    assert!(
        e.execute("INSERT INTO ro VALUES (2)").is_err(),
        "the setting has to reach the transaction, not just the report — a build \
         that fixed only the report would still accept this write"
    );
    e.execute("ROLLBACK").unwrap();
}

/// The two report surfaces must agree, without this test naming a value.
/// `transaction_isolation` had FOUR of them and two disagreed; that was
/// found by an agreement pin, not by counting.
#[test]
fn both_read_only_report_surfaces_agree() {
    let mut e = setup();
    for state in ["off", "on"] {
        e.execute(&format!("SET default_transaction_read_only = {state}"))
            .unwrap();
        for open in [false, true] {
            if open {
                e.execute("BEGIN").unwrap();
            }
            let show = row_text(&mut e, "SHOW transaction_read_only");
            let cs = row_text(&mut e, "SELECT current_setting('transaction_read_only')");
            assert_eq!(show, cs, "SHOW says {show:?}, current_setting says {cs:?}");
            if open {
                e.execute("ROLLBACK").unwrap();
            }
        }
    }
}

/// `SET SESSION CHARACTERISTICS AS TRANSACTION …` was accepted and
/// discarded — pg_dump prepends it to fix the mode for a restore
/// session, so the restore ran under a mode nobody chose.
#[test]
fn set_session_characteristics_sets_both_modes() {
    let mut e = setup();
    e.execute("SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .unwrap();
    assert_eq!(
        row_text(&mut e, "SHOW transaction_isolation"),
        "repeatable read"
    );

    e.execute("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY")
        .unwrap();
    assert_eq!(row_text(&mut e, "SHOW transaction_read_only"), "on");
    e.execute("BEGIN").unwrap();
    assert!(e.execute("INSERT INTO ro VALUES (2)").is_err());
    e.execute("ROLLBACK").unwrap();

    e.execute("SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE")
        .unwrap();
    assert_eq!(row_text(&mut e, "SHOW transaction_read_only"), "off");

    // An explicit mode still outranks the session default.
    e.execute("BEGIN READ ONLY").unwrap();
    assert_eq!(row_text(&mut e, "SHOW transaction_read_only"), "on");
    e.execute("ROLLBACK").unwrap();
}
