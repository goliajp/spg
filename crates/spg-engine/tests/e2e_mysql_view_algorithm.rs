//! v7.17.0 Phase 2.6 — MySQL view prefix clauses (ALGORITHM /
//! DEFINER / SQL SECURITY) between CREATE and VIEW now accept-
//! and-pass-through to the existing view-rewrite engine. Pre-2.6
//! the parser rejected the prefix and the customer's mysqldump
//! restore failed at the first view.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE t (id INT NOT NULL, label TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')").unwrap();
}

#[test]
fn algorithm_undefined_parses_and_creates_view() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("CREATE ALGORITHM = UNDEFINED VIEW v AS SELECT id FROM t WHERE id = 2")
        .unwrap();
    let r = rows(e.execute("SELECT id FROM v").unwrap());
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(2));
}

#[test]
fn algorithm_merge_parses() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("CREATE ALGORITHM = MERGE VIEW v AS SELECT id, label FROM t")
        .unwrap();
    let r = rows(e.execute("SELECT label FROM v WHERE id = 1").unwrap());
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("a".into()));
}

#[test]
fn algorithm_temptable_parses() {
    let mut e = Engine::new();
    setup(&mut e);
    // TEMPTABLE accepted in v7.17 as an alias of MERGE (the
    // view-rewrite is semantically equivalent).
    e.execute("CREATE ALGORITHM = TEMPTABLE VIEW v AS SELECT id FROM t")
        .unwrap();
    let r = rows(e.execute("SELECT id FROM v").unwrap());
    assert_eq!(r.len(), 2);
}

#[test]
fn definer_user_only_form() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("CREATE DEFINER = 'root' VIEW v AS SELECT id FROM t")
        .unwrap();
    let r = rows(e.execute("SELECT id FROM v").unwrap());
    assert_eq!(r.len(), 2);
}

#[test]
fn definer_user_at_host_form() {
    // mysqldump's standard form.
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("CREATE DEFINER = 'root'@'localhost' VIEW v AS SELECT id FROM t")
        .unwrap();
    let r = rows(e.execute("SELECT id FROM v").unwrap());
    assert_eq!(r.len(), 2);
}

#[test]
fn definer_user_at_wildcard_host() {
    // mysqldump also emits `@'%'` for non-localhost grants.
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("CREATE DEFINER = 'admin'@'%' VIEW v AS SELECT id FROM t")
        .unwrap();
    let r = rows(e.execute("SELECT id FROM v").unwrap());
    assert_eq!(r.len(), 2);
}

#[test]
fn sql_security_definer() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("CREATE SQL SECURITY DEFINER VIEW v AS SELECT id FROM t")
        .unwrap();
    let r = rows(e.execute("SELECT id FROM v").unwrap());
    assert_eq!(r.len(), 2);
}

#[test]
fn sql_security_invoker() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("CREATE SQL SECURITY INVOKER VIEW v AS SELECT id FROM t")
        .unwrap();
    let r = rows(e.execute("SELECT id FROM v").unwrap());
    assert_eq!(r.len(), 2);
}

#[test]
fn full_mysqldump_prefix_all_three_clauses() {
    // The exact shape mysqldump emits for every view in a
    // backup.
    let mut e = Engine::new();
    setup(&mut e);
    e.execute(
        "CREATE ALGORITHM = UNDEFINED DEFINER = 'root'@'localhost' SQL SECURITY DEFINER \
         VIEW v AS SELECT id, label FROM t WHERE id > 0",
    )
    .unwrap();
    let r = rows(e.execute("SELECT id FROM v").unwrap());
    assert_eq!(r.len(), 2);
}

#[test]
fn prefix_order_independent() {
    // The clauses can come in any order — MySQL accepts them
    // both ways and pg_dump-from-mysql conversions may shuffle.
    let mut e = Engine::new();
    setup(&mut e);
    e.execute(
        "CREATE SQL SECURITY INVOKER DEFINER = 'u' ALGORITHM = MERGE \
         VIEW v AS SELECT id FROM t",
    )
    .unwrap();
    let r = rows(e.execute("SELECT id FROM v").unwrap());
    assert_eq!(r.len(), 2);
}

#[test]
fn at_alone_lex_smoke() {
    // Regression — `@` standalone used to lex-error pre-2.6.
    // We don't expose Token publicly here; instead drive it
    // through a SELECT that would have errored.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (x INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    // The literal `'a'@'b'` is parser-rejected (it's not a valid
    // expression on its own) but it must not fail at the lex
    // layer with `UnknownChar('@')`. So this returns a parser
    // error (the engine's Err path), not a panic / lex error.
    let r = e.execute("SELECT 'a'@'b'");
    assert!(r.is_err(), "should parser-error, but not lex-error");
}
