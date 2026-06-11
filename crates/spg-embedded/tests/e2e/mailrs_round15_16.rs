//! mailrs rounds 15 + 16 — first hours of real production traffic.
//! Notes: `.claude/notes/mailrs-embed-round15-string-to-array.md`,
//! `.claude/notes/mailrs-embed-round16-runtime-errors-and-trigger-eval.md`.

use spg_embedded::{Database, QueryResult, Value};

fn rows_of(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match db.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("expected rows for {sql}, got {other:?}"),
    }
}

/// Round-16 A — the calendar feed worker's exact ORDER BY shape.
#[test]
fn order_by_nulls_first_last() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE feeds (id BIGINT, last_synced_at BIGINT)")
        .unwrap();
    db.execute("INSERT INTO feeds VALUES (1, 200), (2, NULL), (3, 100)")
        .unwrap();
    let ids = |rows: Vec<Vec<Value>>| -> Vec<i64> {
        rows.iter()
            .map(|r| match r[0] {
                Value::BigInt(n) => n,
                ref o => panic!("{o:?}"),
            })
            .collect()
    };
    // Prod statement: ASC NULLS FIRST flips the ASC default.
    let r = rows_of(
        &mut db,
        "SELECT id FROM feeds ORDER BY last_synced_at ASC NULLS FIRST LIMIT 10",
    );
    assert_eq!(ids(r), vec![2, 3, 1]);
    // PG defaults: NULLS LAST for ASC, NULLS FIRST for DESC.
    let r = rows_of(&mut db, "SELECT id FROM feeds ORDER BY last_synced_at");
    assert_eq!(ids(r), vec![3, 1, 2]);
    let r = rows_of(&mut db, "SELECT id FROM feeds ORDER BY last_synced_at DESC");
    assert_eq!(ids(r), vec![2, 1, 3]);
    let r = rows_of(
        &mut db,
        "SELECT id FROM feeds ORDER BY last_synced_at DESC NULLS LAST",
    );
    assert_eq!(ids(r), vec![1, 3, 2]);
}

/// Round-16 A — the conversation-search aggregate-internal ORDER BY.
#[test]
fn array_agg_internal_order_by_with_nulls_placement() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE msgs (lvl TEXT, score BIGINT)")
        .unwrap();
    db.execute("INSERT INTO msgs VALUES ('high', 90), ('low', 10), ('mid', NULL), ('top', 95)")
        .unwrap();
    let r = rows_of(
        &mut db,
        "SELECT array_agg(lvl ORDER BY score DESC NULLS LAST) FROM msgs",
    );
    match &r[0][0] {
        Value::TextArray(items) => {
            let got: Vec<&str> = items.iter().map(|o| o.as_deref().unwrap()).collect();
            assert_eq!(got, vec!["top", "high", "low", "mid"]);
        }
        other => panic!("expected TextArray, got {other:?}"),
    }
}

/// Round-16 B — the content worker's exact batch-pick statement:
/// correlated NOT EXISTS under WHERE, on the prepared-on-snapshot
/// (sqlx readonly-inline) path AND under a JOIN.
#[test]
fn correlated_not_exists_on_readonly_and_join_paths() {
    let mut db = Database::open_in_memory();
    db.execute(
        "CREATE TABLE messages (id BIGSERIAL PRIMARY KEY, size BIGINT NOT NULL, fid BIGINT)",
    )
    .unwrap();
    db.execute("CREATE TABLE attachment_content (message_id BIGINT NOT NULL)")
        .unwrap();
    db.execute("CREATE TABLE folders (id BIGINT PRIMARY KEY, name TEXT)")
        .unwrap();
    db.execute("INSERT INTO messages (size, fid) VALUES (10, 1), (0, 1), (20, 1)")
        .unwrap();
    db.execute("INSERT INTO attachment_content VALUES (1)")
        .unwrap();
    db.execute("INSERT INTO folders VALUES (1, 'inbox')")
        .unwrap();

    let sql = "SELECT m.id FROM messages m WHERE m.size > 0 AND NOT EXISTS \
               (SELECT 1 FROM attachment_content ac WHERE ac.message_id = m.id) \
               ORDER BY m.id DESC LIMIT $1";
    let stmt = db.prepare(sql).unwrap();
    let snap = db.engine().clone_snapshot();
    let r = Database::execute_prepared_on_snapshot(&snap, &stmt, &[Value::BigInt(5)]).unwrap();
    match r {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].values[0], Value::BigInt(3));
        }
        other => panic!("{other:?}"),
    }

    // JOIN + correlated EXISTS (the prod statement's full shape) —
    // previously "subquery reached row eval — engine resolver bug".
    let r = rows_of(
        &mut db,
        "SELECT m.id, f.name FROM messages m JOIN folders f ON f.id = m.fid \
         WHERE m.size > 0 AND NOT EXISTS \
         (SELECT 1 FROM attachment_content ac WHERE ac.message_id = m.id) \
         ORDER BY m.id DESC",
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::BigInt(3));

    // Select-list correlated subquery.
    let r = rows_of(
        &mut db,
        "SELECT m.id, (SELECT COUNT(*) FROM attachment_content ac \
         WHERE ac.message_id = m.id) FROM messages m ORDER BY m.id",
    );
    assert_eq!(r.len(), 3);
    // COUNT(*) in a scalar subquery surfaces as Int on this path.
    assert_eq!(r[0][1], Value::Int(1));
    assert_eq!(r[1][1], Value::Int(0));
}

/// Round-16 collateral #1 — multi-row VALUES drew the SAME serial id
/// for every row (max+1 computed before any insertion).
#[test]
fn multi_row_insert_increments_serial() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (id BIGSERIAL PRIMARY KEY, v BIGINT)")
        .unwrap();
    db.execute("INSERT INTO t (v) VALUES (10), (20), (30)")
        .unwrap();
    let r = rows_of(&mut db, "SELECT id FROM t ORDER BY id");
    assert_eq!(
        r.iter().map(|x| x[0].clone()).collect::<Vec<_>>(),
        vec![Value::BigInt(1), Value::BigInt(2), Value::BigInt(3)]
    );
    // And the next statement continues the numbering.
    db.execute("INSERT INTO t (v) VALUES (40)").unwrap();
    let r = rows_of(&mut db, "SELECT MAX(id) FROM t");
    assert_eq!(r[0][0], Value::BigInt(4));
}

/// Round-16 collateral #2 — inline `PRIMARY KEY` built the implicit
/// index but never registered uniqueness; duplicates sailed through.
#[test]
fn inline_primary_key_enforces_uniqueness() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
    let err = db.execute("INSERT INTO t VALUES (1, 'dup')").unwrap_err();
    assert!(err.to_string().contains("PRIMARY KEY violation"), "{err}");
    // Batch-internal duplicates too.
    let err = db
        .execute("INSERT INTO t VALUES (7, 'x'), (7, 'y')")
        .unwrap_err();
    assert!(err.to_string().contains("PRIMARY KEY violation"), "{err}");
}

/// Round-15 — migrate-044's exact statement.
#[test]
fn string_to_array_pg_semantics() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (v TEXT NOT NULL)").unwrap();
    db.execute("INSERT INTO t VALUES ('a,b,c')").unwrap();
    db.execute("ALTER TABLE t ALTER COLUMN v TYPE TEXT[] USING string_to_array(v, ',')")
        .unwrap();
    let r = rows_of(&mut db, "SELECT v FROM t");
    assert_eq!(
        r[0][0],
        Value::TextArray(vec![Some("a".into()), Some("b".into()), Some("c".into())])
    );
    // PG 9.1+ semantics row.
    let r = rows_of(
        &mut db,
        "SELECT string_to_array('', ','), string_to_array(NULL, ','), string_to_array('ab', NULL)",
    );
    assert_eq!(r[0][0], Value::TextArray(Vec::new()));
    assert_eq!(r[0][1], Value::Null);
    assert_eq!(
        r[0][2],
        Value::TextArray(vec![Some("a".into()), Some("b".into())])
    );
}

/// Round-16 C — migrate-016's search trigger, verbatim shape:
/// setweight + tsvector concatenation inside a plpgsql trigger body.
#[test]
fn setweight_tsvector_concat_trigger_fires() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE m (subject TEXT, body TEXT, sv tsvector)")
        .unwrap();
    db.execute(
        "CREATE OR REPLACE FUNCTION msv() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
         NEW.sv := setweight(to_tsvector('simple', COALESCE(NEW.subject, '')), 'A') || \
                   setweight(to_tsvector('simple', COALESCE(NEW.body, '')), 'C'); \
         RETURN NEW; END; $$",
    )
    .unwrap();
    db.execute(
        "CREATE TRIGGER tr BEFORE INSERT OR UPDATE ON m FOR EACH ROW EXECUTE FUNCTION msv()",
    )
    .unwrap();
    db.execute("INSERT INTO m (subject, body) VALUES ('urgent invoice', 'please pay the invoice')")
        .unwrap();
    let r = rows_of(&mut db, "SELECT sv FROM m");
    let Value::TsVector(lexemes) = &r[0][0] else {
        panic!("expected tsvector, got {:?}", r[0][0]);
    };
    // subject lexemes carry weight A (3), body-only lexemes C (1);
    // "invoice" appears in both — the stronger label wins.
    let weight_of = |w: &str| lexemes.iter().find(|l| l.word == w).map(|l| l.weight);
    assert_eq!(weight_of("urgent"), Some(3));
    assert_eq!(weight_of("invoice"), Some(3));
    assert_eq!(weight_of("pay"), Some(1));
    // And the weighted vector ranks via the search path.
    let r = rows_of(
        &mut db,
        "SELECT COUNT(*) FROM m WHERE sv @@ plainto_tsquery('simple', 'invoice')",
    );
    assert_eq!(r[0][0], Value::BigInt(1));
}

/// Round-16 D — triggers are introspectable: pg_trigger reports
/// registration + enabled state, so "is the dump's trigger live"
/// is a one-line health check instead of a silent mystery.
#[test]
fn pg_trigger_reports_registration_and_enabled_state() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE m (s TEXT)").unwrap();
    db.execute(
        "CREATE FUNCTION f() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END; $$",
    )
    .unwrap();
    db.execute("CREATE TRIGGER tr BEFORE INSERT OR UPDATE ON m FOR EACH ROW EXECUTE FUNCTION f()")
        .unwrap();
    let r = rows_of(
        &mut db,
        "SELECT tgname, relname, tgenabled, events, function FROM pg_trigger",
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("tr".into()));
    assert_eq!(r[0][2], Value::Text("O".into()));
    assert_eq!(r[0][3], Value::Text("INSERT OR UPDATE".into()));
    db.execute("ALTER TABLE m DISABLE TRIGGER tr").unwrap();
    let r = rows_of(
        &mut db,
        "SELECT tgenabled FROM pg_trigger WHERE tgname = 'tr'",
    );
    assert_eq!(r[0][0], Value::Text("D".into()));
}
