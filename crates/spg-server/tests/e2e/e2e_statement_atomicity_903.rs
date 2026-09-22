//! 9.0.3 — a statement that failed after writing left what it had
//! written, in autocommit.
//!
//! Measured on the published 9.0.3 against PostgreSQL 18.6, with an AFTER
//! trigger that raises on the second row:
//!
//! ```text
//!   INSERT INTO ta VALUES (1, 10), (2, 20)    SPG: both rows stay   PG: none
//!   UPDATE ta SET v = v + 1                   SPG: all rows changed PG: none
//! ```
//!
//! And what stayed did not reach the log: a failed statement's redo is
//! discarded, so those rows were in the running server and not in the WAL.
//!
//! The same run measured two more: the trigger's `RAISE EXCEPTION` reached
//! the client as `42000 trigger function "boom": RAISE EXCEPTION "boom on
//! 2"` where PostgreSQL sends `P0001 boom on 2`, and a serial column
//! handed the same id out twice across a ROLLBACK.

use crate::common;
use crate::e2e_copy_903::{ok, open, rows, run};

fn server(name: &str) -> (common::ChildGuard, std::net::TcpStream) {
    let dir = crate::common::tmp_base().join(format!("spg-e2e-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    let child = common::ChildGuard(raw);
    let c = open(addrs.pgwire.as_ref().unwrap());
    (child, c)
}

#[test]
fn a_statement_that_fails_after_writing_leaves_nothing() {
    let (_child, mut c) = server("atom903");
    ok(&mut c, "CREATE TABLE ta (id int PRIMARY KEY, v int)");
    ok(
        &mut c,
        "CREATE FUNCTION boom() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
         IF NEW.id = 2 THEN RAISE EXCEPTION 'boom on %', NEW.id; END IF; RETURN NEW; END $$",
    );
    ok(
        &mut c,
        "CREATE TRIGGER tb AFTER INSERT OR UPDATE ON ta FOR EACH ROW EXECUTE FUNCTION boom()",
    );

    // PostgreSQL: P0001, the message the block raised, and no row.
    let r = run(&mut c, "INSERT INTO ta VALUES (1, 10), (2, 20)", "");
    assert_eq!(r.error.as_deref(), Some("P0001: boom on 2"));
    assert_eq!(rows(&mut c, "SELECT count(*) FROM ta"), "0\n");

    ok(&mut c, "INSERT INTO ta VALUES (1, 10), (3, 30)");
    let r = run(
        &mut c,
        "UPDATE ta SET v = v + 1, id = CASE WHEN id = 3 THEN 2 ELSE id END",
        "",
    );
    assert_eq!(r.error.as_deref(), Some("P0001: boom on 2"));
    assert_eq!(
        rows(&mut c, "SELECT id, v FROM ta ORDER BY id"),
        "1|10\n3|30\n"
    );

    // …and the rows that stayed are the rows the log holds: a restart
    // answers the same.
    ok(&mut c, "CHECKPOINT");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM ta"), "2\n");
}

#[test]
fn a_rolled_back_statement_still_consumes_its_ids() {
    let (_child, mut c) = server("seqrb903");
    ok(
        &mut c,
        "CREATE TABLE s1 (id serial PRIMARY KEY, v int CHECK (v > 0))",
    );
    ok(&mut c, "CREATE SEQUENCE sq");
    // PostgreSQL 18.6: 1, 2, then the serial column's next id is 2.
    assert_eq!(rows(&mut c, "BEGIN"), "");
    assert_eq!(rows(&mut c, "SELECT nextval('sq')"), "1\n");
    ok(&mut c, "INSERT INTO s1 (v) VALUES (1)");
    ok(&mut c, "ROLLBACK");
    assert_eq!(rows(&mut c, "SELECT nextval('sq')"), "2\n");
    ok(&mut c, "INSERT INTO s1 (v) VALUES (1)");
    assert_eq!(rows(&mut c, "SELECT id FROM s1"), "2\n");
    // The same for a statement that fails on its own.
    let r = run(&mut c, "INSERT INTO s1 (v) VALUES (0)", "");
    assert!(
        r.error.as_deref().is_some_and(|e| e.starts_with("23514:")),
        "{:?}",
        r.error
    );
    ok(&mut c, "INSERT INTO s1 (v) VALUES (2)");
    assert_eq!(rows(&mut c, "SELECT id FROM s1 ORDER BY id"), "2\n4\n");
}
