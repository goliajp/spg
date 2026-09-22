//! 9.0.3 — MERGE wrote its rows straight to storage, which checks only
//! the column types and NOT NULL. Measured against PostgreSQL 18.6:
//!
//! ```text
//!   a table with a serial id     MERGE … INSERT (pid, v) → null value in column "id"
//!   a UNIQUE column              a second row with the same value went in
//!   CHECK (v >= 0)               WHEN MATCHED THEN UPDATE SET v = -1 stored -1
//!   a FOREIGN KEY                a child row with no parent went in
//!   an audit trigger             saw a MERGE's UPDATE and DELETE not at all
//! ```
//!
//! Its INSERT action runs through the INSERT executor now, and its
//! UPDATE action through the UPDATE checks; both fire their row
//! triggers. Every expected value below is PostgreSQL 18.6's.

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
fn merge_writes_through_the_insert_and_update_pipelines() {
    let (_child, mut c) = server("merge903");
    ok(&mut c, "CREATE TABLE p (id int PRIMARY KEY)");
    ok(&mut c, "INSERT INTO p VALUES (1)");
    ok(
        &mut c,
        "CREATE TABLE ch (id serial PRIMARY KEY, pid int REFERENCES p(id), \
         v int CHECK (v >= 0), g int GENERATED ALWAYS AS (v * 2) STORED, note text DEFAULT 'd')",
    );

    // A parent that does not exist, and a CHECK the row breaks: PG's
    // messages, and nothing written.
    let r = run(
        &mut c,
        "MERGE INTO ch USING (VALUES (99, 5)) s(pid, v) ON false \
         WHEN NOT MATCHED THEN INSERT (pid, v) VALUES (s.pid, s.v)",
        "",
    );
    assert!(
        r.error.as_deref().is_some_and(|e| e.starts_with("23503:")),
        "{:?}",
        r.error
    );
    let r = run(
        &mut c,
        "MERGE INTO ch USING (VALUES (1, -5)) s(pid, v) ON false \
         WHEN NOT MATCHED THEN INSERT (pid, v) VALUES (s.pid, s.v)",
        "",
    );
    assert!(
        r.error.as_deref().is_some_and(|e| e.starts_with("23514:")),
        "{:?}",
        r.error
    );
    assert_eq!(rows(&mut c, "SELECT count(*) FROM ch"), "0\n");

    // The omitted columns take their defaults: the serial's id, the
    // generated column, and `note`.
    ok(
        &mut c,
        "MERGE INTO ch USING (VALUES (1, 7)) s(pid, v) ON false \
         WHEN NOT MATCHED THEN INSERT (pid, v) VALUES (s.pid, s.v)",
    );
    assert_eq!(rows(&mut c, "SELECT pid, v, g, note FROM ch"), "1|7|14|d\n");

    // The UPDATE action is checked too.
    let r = run(
        &mut c,
        "MERGE INTO ch USING (VALUES (1)) s(pid) ON ch.pid = s.pid \
         WHEN MATCHED THEN UPDATE SET v = -1",
        "",
    );
    assert!(
        r.error.as_deref().is_some_and(|e| e.starts_with("23514:")),
        "{:?}",
        r.error
    );
    assert_eq!(rows(&mut c, "SELECT v FROM ch"), "7\n");
}

#[test]
fn merge_refuses_a_duplicate_and_fires_its_row_triggers() {
    let (_child, mut c) = server("mergetrg903");
    ok(
        &mut c,
        "CREATE TABLE ma (id int PRIMARY KEY, v int NOT NULL, code text UNIQUE)",
    );
    ok(
        &mut c,
        "INSERT INTO ma VALUES (1, 10, 'one'), (2, 20, 'two')",
    );
    // PostgreSQL: 23505 on the INSERT action, and the MATCHED update
    // that ran first is rolled back with it.
    let r = run(
        &mut c,
        "MERGE INTO ma t USING (VALUES (1, 'x'), (3, 'two')) s(id, c) ON t.id = s.id \
         WHEN MATCHED THEN UPDATE SET v = t.v + 1 \
         WHEN NOT MATCHED THEN INSERT (id, v, code) VALUES (s.id, 0, s.c)",
        "",
    );
    assert!(
        r.error.as_deref().is_some_and(|e| e.starts_with("23505:")),
        "{:?}",
        r.error
    );
    assert_eq!(
        rows(&mut c, "SELECT id, v, code FROM ma ORDER BY id"),
        "1|10|one\n2|20|two\n"
    );

    // All three actions fire their row triggers, as PostgreSQL does.
    ok(&mut c, "CREATE TABLE m (id int PRIMARY KEY, v int)");
    ok(&mut c, "INSERT INTO m VALUES (1, 10), (2, 20)");
    ok(&mut c, "CREATE TABLE audit (msg text)");
    ok(
        &mut c,
        "CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
         INSERT INTO audit VALUES (TG_OP || ' ' || COALESCE(OLD.id::text,'-') || '->' \
         || COALESCE(NEW.id::text,'-')); RETURN COALESCE(NEW, OLD); END $$",
    );
    ok(
        &mut c,
        "CREATE TRIGGER t_all AFTER INSERT OR UPDATE OR DELETE ON m \
         FOR EACH ROW EXECUTE FUNCTION trg()",
    );
    ok(
        &mut c,
        "MERGE INTO m t USING (VALUES (1), (2), (3)) s(id) ON t.id = s.id \
         WHEN MATCHED AND s.id = 1 THEN UPDATE SET v = t.v + 1 \
         WHEN MATCHED AND s.id = 2 THEN DELETE \
         WHEN NOT MATCHED THEN INSERT (id, v) VALUES (s.id, 0)",
    );
    assert_eq!(
        rows(&mut c, "SELECT msg FROM audit ORDER BY msg"),
        "DELETE 2->-\nINSERT -->3\nUPDATE 1->1\n"
    );
    assert_eq!(
        rows(&mut c, "SELECT id, v FROM m ORDER BY id"),
        "1|11\n3|0\n"
    );
}
