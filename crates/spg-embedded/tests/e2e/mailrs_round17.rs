//! mailrs embed round-17 — the four parser gaps under the INBOX
//! list + search queries (prod inbox alive only via the kevy
//! cache). Note: `.claude/notes/mailrs-embed-round17-inbox-query-shapes.md`.

use spg_embedded::{Database, QueryResult, Value};

fn rows_of(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match db.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("expected rows for {sql}, got {other:?}"),
    }
}

/// Gap 1 — ILIKE / NOT ILIKE.
#[test]
fn ilike_matches_case_insensitively() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (s TEXT)").unwrap();
    db.execute("INSERT INTO t VALUES ('Hello World'), ('goodbye'), (NULL)")
        .unwrap();
    let r = rows_of(&mut db, "SELECT s FROM t WHERE s ILIKE '%hello%'");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("Hello World".into()));
    // NOT ILIKE excludes the match AND the NULL (PG semantics).
    let r = rows_of(&mut db, "SELECT s FROM t WHERE s NOT ILIKE '%HELLO%'");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("goodbye".into()));
    // Plain LIKE stays case-sensitive.
    let r = rows_of(&mut db, "SELECT COUNT(*) FROM t WHERE s LIKE '%hello%'");
    assert_eq!(r[0][0], Value::BigInt(0));
}

/// Gap 2 — DISTINCT inside aggregates, incl. mailrs's real
/// COUNT(DISTINCT CASE …) inbox-counter shape.
#[test]
fn distinct_aggregates() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE m (thread TEXT, sender TEXT, unread BIGINT)")
        .unwrap();
    db.execute(
        "INSERT INTO m VALUES ('t1','alice',1),('t1','alice',0),('t1','bob',1),('t2','carol',0)",
    )
    .unwrap();
    let r = rows_of(&mut db, "SELECT COUNT(DISTINCT sender) FROM m");
    assert_eq!(r[0][0], Value::BigInt(3));
    let r = rows_of(
        &mut db,
        "SELECT string_agg(DISTINCT sender, ',' ORDER BY sender) FROM m WHERE thread = 't1'",
    );
    assert_eq!(r[0][0], Value::Text("alice,bob".into()));
    // The inbox unread-counter shape: DISTINCT over a CASE arm.
    let r = rows_of(
        &mut db,
        "SELECT thread, COUNT(DISTINCT CASE WHEN unread = 1 THEN sender END) \
         FROM m GROUP BY thread ORDER BY thread",
    );
    assert_eq!(r[0][1], Value::BigInt(2)); // t1: alice + bob unread
    assert_eq!(r[1][1], Value::BigInt(0)); // t2: none
}

/// Gap 3 — standard CAST(expr AS type), incl. inside DISTINCT CASE.
#[test]
fn cast_function_form() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (n BIGINT)").unwrap();
    db.execute("INSERT INTO t VALUES (1), (2), (2)").unwrap();
    let r = rows_of(&mut db, "SELECT CAST(1 AS TEXT)");
    assert_eq!(r[0][0], Value::Text("1".into()));
    let r = rows_of(
        &mut db,
        "SELECT COUNT(DISTINCT CASE WHEN n > 0 THEN CAST(n AS TEXT) END) FROM t",
    );
    assert_eq!(r[0][0], Value::BigInt(2));
}

/// Gap 4 — a CTE referencing an earlier CTE (the search query's
/// `WITH matched AS (…), cands AS (… IN (SELECT … FROM matched))`).
#[test]
fn cte_chain_references_earlier_cte() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE msgs (id BIGINT, thread BIGINT, body TEXT)")
        .unwrap();
    db.execute("INSERT INTO msgs VALUES (1, 10, 'invoice'), (2, 10, 'other'), (3, 20, 'x')")
        .unwrap();
    let r = rows_of(
        &mut db,
        "WITH a AS (SELECT 1 AS x), b AS (SELECT x FROM a) SELECT x FROM b",
    );
    assert_eq!(r[0][0], Value::Int(1));
    // The real search shape: second CTE filters via IN over the first.
    let r = rows_of(
        &mut db,
        "WITH matched AS (SELECT thread FROM msgs WHERE body ILIKE '%invoice%'), \
         cands AS (SELECT id FROM msgs WHERE thread IN (SELECT thread FROM matched)) \
         SELECT COUNT(*) FROM cands",
    );
    assert_eq!(r[0][0], Value::BigInt(2)); // both messages of thread 10
}

/// The composed inbox-counter query: every gap in one statement.
#[test]
fn inbox_list_shape_composes() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE m (id BIGINT, thread TEXT, sender TEXT, unread BIGINT, subj TEXT)")
        .unwrap();
    db.execute(
        "INSERT INTO m VALUES \
         (1,'t1','alice',1,'Invoice due'),(2,'t1','bob',0,'re: Invoice due'),\
         (3,'t2','carol',1,'hello'),(4,'t2','carol',0,'HELLO again')",
    )
    .unwrap();
    let r = rows_of(
        &mut db,
        "WITH hits AS (SELECT thread FROM m WHERE subj ILIKE '%invoice%'), \
         agg AS (SELECT thread, COUNT(DISTINCT CAST(id AS TEXT)) AS msgs, \
                 COUNT(DISTINCT CASE WHEN unread = 1 THEN sender END) AS unread_senders \
                 FROM m WHERE thread IN (SELECT thread FROM hits) GROUP BY thread) \
         SELECT thread, msgs, unread_senders FROM agg ORDER BY thread",
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("t1".into()));
    assert_eq!(r[0][1], Value::BigInt(2));
    assert_eq!(r[0][2], Value::BigInt(1));
}

/// Round-18 — placeholders and clock calls inside CTE bodies (and
/// the collateral subtrees the shared walker now covers: JOIN ON
/// placeholders, LIMIT $N inside a CTE).
#[test]
fn round18_placeholders_and_clock_inside_ctes() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (n BIGINT)").unwrap();
    db.execute("INSERT INTO t VALUES (1), (2)").unwrap();
    // The verbatim round-18 repro.
    let stmt = db
        .prepare("WITH a AS (SELECT n FROM t WHERE n = $1) SELECT n FROM a")
        .unwrap();
    let r = db.execute_prepared(&stmt, &[Value::BigInt(1)]).unwrap();
    match r {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].values[0], Value::BigInt(1));
        }
        other => panic!("{other:?}"),
    }
    // The clock twin: NOW() inside the CTE body.
    let r = rows_of(
        &mut db,
        "WITH a AS (SELECT n FROM t WHERE n < EXTRACT(EPOCH FROM NOW())) SELECT COUNT(*) FROM a",
    );
    assert_eq!(r[0][0], Value::BigInt(2));
    // JOIN ON placeholder (collateral the shared walker closes).
    db.execute("CREATE TABLE u (n BIGINT)").unwrap();
    db.execute("INSERT INTO u VALUES (1)").unwrap();
    let stmt = db
        .prepare("SELECT t.n FROM t JOIN u ON u.n = t.n AND t.n = $1")
        .unwrap();
    let r = db.execute_prepared(&stmt, &[Value::BigInt(1)]).unwrap();
    match r {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("{other:?}"),
    }
    // LIMIT $N inside the CTE body.
    let stmt = db
        .prepare("WITH a AS (SELECT n FROM t ORDER BY n LIMIT $1) SELECT COUNT(*) FROM a")
        .unwrap();
    let r = db.execute_prepared(&stmt, &[Value::BigInt(1)]).unwrap();
    match r {
        QueryResult::Rows { rows, .. } => assert_eq!(rows[0].values[0], Value::BigInt(1)),
        other => panic!("{other:?}"),
    }
}
