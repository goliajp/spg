//! v7.39 (read01 round 59) — column-level privileges: `GRANT SELECT (col)`,
//! `pg_attribute.attacl`, and column-scoped enforcement.
//!
//! The last of round 57's four residuals. A column grant is a genuinely
//! different thing from a table grant: it lives in the COLUMN's own ACL, never
//! touches `relacl`, and lets a role read two columns of a table it has no
//! table-wide SELECT on at all.
//!
//! The rules that are easy to get wrong, all byte-locked against live PG18.4:
//!   - `SELECT *` reaches every column, so it needs every column granted — it
//!     is denied even though the role can read most of them.
//!   - `SELECT count(*)` reads no column VALUE, so ANY column privilege carries
//!     it. This is the one place "reads the table but reads no column" matters.
//!   - A column named in a WHERE is read just as much as one in the SELECT list.

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

fn r1(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    ok(&mut e, "CREATE ROLE dan LOGIN PASSWORD 'x'");
    ok(&mut e, "CREATE TABLE ct (id int, secret text, pub text)");
    ok(&mut e, "INSERT INTO ct VALUES (1,'s','p')");
    ok(&mut e, "GRANT SELECT (id, pub) ON ct TO dan");
    e
}

#[test]
fn a_column_grant_lands_in_attacl_and_leaves_relacl_alone() {
    let mut e = seeded();
    assert_eq!(
        r1(
            &mut e,
            "SELECT coalesce(relacl,'NULL') FROM pg_class WHERE relname='ct'"
        ),
        "NULL"
    );
    assert_eq!(
        col(
            &mut e,
            "SELECT attname||'='||coalesce(attacl,'NULL') FROM pg_attribute \
             WHERE attrelid='ct'::regclass AND attnum>0 ORDER BY attnum"
        ),
        ["id={dan=r/admin}", "secret=NULL", "pub={dan=r/admin}"]
    );
    // Revoking takes the column's entry away again — back to NULL.
    ok(&mut e, "REVOKE SELECT (pub) ON ct FROM dan");
    assert_eq!(
        col(
            &mut e,
            "SELECT coalesce(attacl,'NULL') FROM pg_attribute \
             WHERE attrelid='ct'::regclass AND attnum>0 ORDER BY attnum"
        ),
        ["{dan=r/admin}", "NULL", "NULL"]
    );
}

#[test]
fn the_privilege_probes_tell_the_truth_about_columns() {
    let mut e = seeded();
    // A column privilege is the table's OR the column's own.
    assert_eq!(
        r1(
            &mut e,
            "SELECT has_column_privilege('dan','ct','pub','SELECT')"
        ),
        "true"
    );
    assert_eq!(
        r1(
            &mut e,
            "SELECT has_column_privilege('dan','ct','secret','SELECT')"
        ),
        "false"
    );
    // The table-wide question stays false — a column grant does not confer it.
    assert_eq!(
        r1(&mut e, "SELECT has_table_privilege('dan','ct','SELECT')"),
        "false"
    );
    // …but "can this role touch the table at all?" is true.
    assert_eq!(
        r1(
            &mut e,
            "SELECT has_any_column_privilege('dan','ct','SELECT')"
        ),
        "true"
    );
}

#[test]
fn only_the_granted_columns_can_be_read() {
    let mut e = seeded();
    ok(&mut e, "SET ROLE dan");
    // The two granted columns: fine.
    assert_eq!(r1(&mut e, "SELECT id FROM ct"), "1");
    assert_eq!(r1(&mut e, "SELECT pub FROM ct"), "p");
    // `SELECT *` reaches EVERY column, secret included, so PG denies it.
    assert_eq!(
        err(&mut e, "SELECT * FROM ct"),
        "unsupported: permission denied for table ct"
    );
    assert_eq!(
        err(&mut e, "SELECT secret FROM ct"),
        "unsupported: permission denied for table ct"
    );
    // A column read in a WHERE is read just the same.
    assert_eq!(
        err(&mut e, "SELECT id FROM ct WHERE secret='s'"),
        "unsupported: permission denied for table ct"
    );
}

#[test]
fn count_star_reads_no_column_so_any_column_privilege_carries_it() {
    let mut e = seeded();
    ok(&mut e, "SET ROLE dan");
    // The one shape where "reads the table, reads no column value" matters.
    assert_eq!(r1(&mut e, "SELECT count(*) FROM ct"), "1");
    ok(&mut e, "RESET ROLE");
    ok(&mut e, "REVOKE SELECT (id, pub) ON ct FROM dan");
    ok(&mut e, "SET ROLE dan");
    // With no column privilege left anywhere, even count(*) is denied.
    assert_eq!(
        err(&mut e, "SELECT count(*) FROM ct"),
        "unsupported: permission denied for table ct"
    );
}

#[test]
fn insert_is_column_scoped_too() {
    let mut e = seeded();
    ok(&mut e, "GRANT INSERT (id) ON ct TO dan");
    ok(&mut e, "SET ROLE dan");
    ok(&mut e, "INSERT INTO ct (id) VALUES (2)");
    // `secret` was never granted for INSERT.
    assert_eq!(
        err(&mut e, "INSERT INTO ct (id, secret) VALUES (3,'x')"),
        "unsupported: permission denied for table ct"
    );
    // An INSERT with no column list names them ALL, so only a table-wide grant
    // could carry it.
    assert_eq!(
        err(&mut e, "INSERT INTO ct VALUES (4,'x','y')"),
        "unsupported: permission denied for table ct"
    );
}

#[test]
fn update_is_column_scoped_too() {
    let mut e = seeded();
    ok(&mut e, "GRANT UPDATE (pub) ON ct TO dan");
    ok(&mut e, "SET ROLE dan");
    // Writing the granted column, reading a granted column in the WHERE.
    ok(&mut e, "UPDATE ct SET pub='q' WHERE id=1");
    // Writing a column that was never granted for UPDATE.
    assert_eq!(
        err(&mut e, "UPDATE ct SET secret='z' WHERE id=1"),
        "unsupported: permission denied for table ct"
    );
    // Reading an ungranted column in the WHERE is denied just the same.
    assert_eq!(
        err(&mut e, "UPDATE ct SET pub='q' WHERE secret='s'"),
        "unsupported: permission denied for table ct"
    );
}

#[test]
fn information_schema_reports_the_column_grants() {
    let mut e = seeded();
    ok(&mut e, "GRANT INSERT (id) ON ct TO dan");
    assert_eq!(
        col(
            &mut e,
            "SELECT column_name||'|'||privilege_type FROM information_schema.column_privileges \
             WHERE table_name='ct' AND grantee='dan' ORDER BY column_name, privilege_type"
        ),
        ["id|INSERT", "id|SELECT", "pub|SELECT"]
    );
}

#[test]
fn granting_on_a_column_that_does_not_exist_is_an_error() {
    let mut e = seeded();
    assert_eq!(
        err(&mut e, "GRANT SELECT (nope) ON ct TO dan"),
        "unsupported: column \"nope\" of relation \"ct\" does not exist"
    );
}

#[test]
fn a_table_wide_grant_still_covers_every_column() {
    // The column gate must not get in the way of the ordinary case.
    let mut e = seeded();
    ok(&mut e, "GRANT SELECT ON ct TO dan");
    ok(&mut e, "SET ROLE dan");
    assert_eq!(r1(&mut e, "SELECT secret FROM ct"), "s");
    assert_eq!(r1(&mut e, "SELECT count(*) FROM ct"), "1");
    assert_eq!(col(&mut e, "SELECT id FROM ct WHERE secret='s'"), ["1"]);
}
