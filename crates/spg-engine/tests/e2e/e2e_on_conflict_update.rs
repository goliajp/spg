//! v7.9.9 — INSERT … ON CONFLICT (col) DO UPDATE SET … with
//! EXCLUDED.col references + RETURNING. mailrs migration
//! blocker #2 (the heavy 47-site half).

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn engine_with(sqls: &[&str]) -> Engine {
    let mut eng = Engine::new();
    for sql in sqls {
        let r = eng
            .execute(sql)
            .unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
        assert!(matches!(r, QueryResult::CommandOk { .. }), "{sql:?}");
    }
    eng
}

fn select(eng: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    match eng.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn upsert_existing_row_via_excluded() {
    // mailrs accounts upsert pattern.
    let mut eng = engine_with(&[
        "CREATE TABLE accounts (id INT NOT NULL, password_hash TEXT NOT NULL)",
        "CREATE INDEX accounts_pk ON accounts (id)",
        "INSERT INTO accounts VALUES (1, 'old_hash')",
    ]);
    eng.execute(
        "INSERT INTO accounts VALUES (1, 'new_hash') \
         ON CONFLICT (id) DO UPDATE SET password_hash = EXCLUDED.password_hash",
    )
    .unwrap();
    let rows = select(&mut eng, "SELECT password_hash FROM accounts");
    assert_eq!(rows[0][0], Value::text("new_hash"));
}

#[test]
fn upsert_inserts_when_no_conflict() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)",
        "CREATE INDEX t_pk ON t (id)",
    ]);
    eng.execute("INSERT INTO t VALUES (1, 100) ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v")
        .unwrap();
    let rows = select(&mut eng, "SELECT v FROM t");
    assert_eq!(rows[0][0], Value::Int(100));
}

#[test]
fn upsert_multi_assignment_with_excluded_refs() {
    let mut eng = engine_with(&[
        "CREATE TABLE calendar_events (uid INT NOT NULL, payload TEXT, etag TEXT)",
        "CREATE INDEX cal_pk ON calendar_events (uid)",
        "INSERT INTO calendar_events VALUES (1, 'v1', 'e1')",
    ]);
    eng.execute(
        "INSERT INTO calendar_events VALUES (1, 'v2', 'e2') \
         ON CONFLICT (uid) DO UPDATE SET payload = EXCLUDED.payload, etag = EXCLUDED.etag",
    )
    .unwrap();
    let rows = select(&mut eng, "SELECT payload, etag FROM calendar_events");
    assert_eq!(rows[0][0], Value::text("v2"));
    assert_eq!(rows[0][1], Value::text("e2"));
}

#[test]
fn upsert_set_can_reference_both_table_and_excluded() {
    // The increment pattern: `SET counter = t.counter + EXCLUDED.counter`.
    let mut eng = engine_with(&[
        "CREATE TABLE counters (id INT NOT NULL, counter INT NOT NULL)",
        "CREATE INDEX counters_pk ON counters (id)",
        "INSERT INTO counters VALUES (1, 5)",
    ]);
    eng.execute(
        "INSERT INTO counters VALUES (1, 3) \
         ON CONFLICT (id) DO UPDATE SET counter = counters.counter + EXCLUDED.counter",
    )
    .unwrap();
    let rows = select(&mut eng, "SELECT counter FROM counters");
    assert_eq!(rows[0][0], Value::Int(8));
}

#[test]
fn upsert_returning_yields_post_update_row() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL, name TEXT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "INSERT INTO u VALUES (1, 'old')",
    ]);
    let r = eng
        .execute(
            "INSERT INTO u VALUES (1, 'new') \
             ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name \
             RETURNING id, name",
        )
        .unwrap();
    let rows = match r {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!(),
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int(1));
    assert_eq!(rows[0].values[1], Value::text("new"));
}

#[test]
fn upsert_with_where_skips_when_false() {
    let mut eng = engine_with(&[
        "CREATE TABLE suppression (address TEXT NOT NULL, reason TEXT)",
        "CREATE INDEX supp_pk ON suppression (address)",
        "INSERT INTO suppression VALUES ('bounced@x.com', 'bounce')",
    ]);
    // WHERE EXCLUDED.reason <> 'unknown' — the incoming reason
    // IS 'unknown', so the update is skipped.
    eng.execute(
        "INSERT INTO suppression VALUES ('bounced@x.com', 'unknown') \
         ON CONFLICT (address) DO UPDATE SET reason = EXCLUDED.reason \
         WHERE EXCLUDED.reason <> 'unknown'",
    )
    .unwrap();
    let rows = select(&mut eng, "SELECT reason FROM suppression");
    assert_eq!(rows[0][0], Value::text("bounce"));
}

#[test]
fn upsert_excluded_inside_case_and_or() {
    // mailrs 7.32.1 contact auto-capture: EXCLUDED refs nested inside a
    // CASE expression and an OR. The CASE arm regressed with
    // "unknown table qualifier: excluded" because substitute_excluded_refs
    // didn't recurse into Case / IsNull / Cast / Like / InList.
    let mut eng = engine_with(&[
        "CREATE TABLE email_contacts (email TEXT NOT NULL, display_name TEXT NOT NULL DEFAULT '', \
         is_mailing_list BOOL NOT NULL DEFAULT false)",
        "CREATE UNIQUE INDEX ec_pk ON email_contacts (email)",
        "INSERT INTO email_contacts VALUES ('a@x.com', 'Old Name', false)",
    ]);
    eng.execute(
        "INSERT INTO email_contacts VALUES ('a@x.com', 'New Name', true) \
         ON CONFLICT (email) DO UPDATE SET \
            display_name = CASE WHEN EXCLUDED.display_name != '' \
                THEN EXCLUDED.display_name ELSE email_contacts.display_name END, \
            is_mailing_list = email_contacts.is_mailing_list OR EXCLUDED.is_mailing_list",
    )
    .unwrap();
    let rows = select(
        &mut eng,
        "SELECT display_name, is_mailing_list FROM email_contacts",
    );
    assert_eq!(rows[0][0], Value::text("New Name"), "CASE picked EXCLUDED");
    assert_eq!(rows[0][1], Value::Bool(true), "OR folded EXCLUDED");

    // The empty-incoming branch keeps the existing name.
    eng.execute(
        "INSERT INTO email_contacts VALUES ('a@x.com', '', false) \
         ON CONFLICT (email) DO UPDATE SET \
            display_name = CASE WHEN EXCLUDED.display_name != '' \
                THEN EXCLUDED.display_name ELSE email_contacts.display_name END",
    )
    .unwrap();
    let rows = select(&mut eng, "SELECT display_name FROM email_contacts");
    assert_eq!(rows[0][0], Value::text("New Name"), "CASE kept existing");
}
