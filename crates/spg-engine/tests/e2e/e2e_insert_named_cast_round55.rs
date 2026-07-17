//! v7.39 (read01, round 55) — closing the round-54 audit: it swept regclass
//! and enum casts across every execution path but never covered the other two
//! members of the catalog-dependent family, composite and domain. They turned
//! up one more instance of the same bug, in the one path the sweep had missed.
//!
//! `literal_expr_to_value` — which is how an INSERT evaluates its VALUES —
//! took no catalog at all. Its Cast arm called `cast_value(v, target)`, which
//! cannot resolve a user-named type on its own, so ANY user-named cast in an
//! INSERT's VALUES failed the whole statement with "unsupported cast target".

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn insert_values_resolve_a_domain_cast() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE DOMAIN posint AS int CHECK (VALUE > 0)");
    ok(&mut e, "CREATE TABLE dm(a posint, t text)");
    // Used to fail the whole INSERT with "unsupported cast target `::posint`".
    ok(&mut e, "INSERT INTO dm VALUES (5::posint, 'x')");
    ok(&mut e, "INSERT INTO dm VALUES (3, 'y')");
    assert_eq!(col(&mut e, "SELECT count(*) FROM dm"), vec!["2"]);
    assert_eq!(
        col(&mut e, "SELECT count(*) FROM dm WHERE a = 5::posint"),
        vec!["1"]
    );
    // The domain's CHECK still fires on a cast value.
    assert!(
        e.execute("INSERT INTO dm VALUES (-1::posint, 'bad')")
            .is_err()
    );
}

#[test]
fn insert_values_resolve_an_enum_cast() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TYPE mood AS ENUM ('sad','ok')");
    ok(&mut e, "CREATE TABLE dm(m mood, t text)");
    ok(&mut e, "INSERT INTO dm VALUES ('ok'::mood, 'x')");
    ok(&mut e, "INSERT INTO dm VALUES ('sad', 'y')");
    assert_eq!(
        col(&mut e, "SELECT count(*) FROM dm WHERE m = 'ok'::mood"),
        vec!["1"]
    );
    ok(&mut e, "UPDATE dm SET t = 'z' WHERE m = 'sad'::mood");
    assert_eq!(
        col(&mut e, "SELECT t FROM dm WHERE m = 'sad'::mood"),
        vec!["z"]
    );
}

#[test]
fn insert_values_resolve_a_composite_cast() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TYPE pt AS (x int, y int)");
    ok(&mut e, "CREATE TABLE dc(p pt, t text)");
    // The cast resolves and the value lands. SPG stores a composite-typed
    // column as JSON, so the Composite coerces into it. (Recorded epic: the
    // composite OPERATIONS — field access `(p).x`, `= ROW(...)`, ordering —
    // are not implemented on that representation.)
    ok(&mut e, "INSERT INTO dc VALUES (ROW(1,2)::pt, 'a')");
    assert_eq!(col(&mut e, "SELECT count(*) FROM dc"), vec!["1"]);
}
