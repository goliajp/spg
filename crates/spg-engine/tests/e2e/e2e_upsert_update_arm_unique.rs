//! 8.0.3 — the upsert's UPDATE arm is held to the table's unique rules.
//!
//! A plain `UPDATE` checked every unique constraint and unique index;
//! `ON CONFLICT … DO UPDATE` checked only its arbiter, so the update it
//! queued could write a value another row's unique key already held and
//! the table ended up violating its own constraint. Reported by sentori,
//! identical on every build back to 7.38.6. Every expectation here is
//! PostgreSQL 18.6's answer for the same statements.

use spg_engine::{Engine, QueryResult};

fn setup(sqls: &[&str]) -> Engine {
    let mut e = Engine::new();
    for s in sqls {
        e.execute(s).unwrap_or_else(|x| panic!("{s}: {x:?}"));
    }
    e
}

fn rows(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join(":")
            })
            .collect::<Vec<_>>()
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

fn refused_with(e: &mut Engine, sql: &str, want: &str) {
    let err = e.execute(sql).expect_err(sql);
    let text = format!("{err}");
    assert!(
        text.contains(want),
        "{sql}\n  want {want:?}\n  got  {text:?}"
    );
}

#[test]
fn an_int_unique_constraint_refuses_the_update_arm() {
    let mut e = setup(&[
        "CREATE TABLE du_i (id int PRIMARY KEY, k int UNIQUE)",
        "INSERT INTO du_i VALUES (1, 10), (2, 20)",
    ]);
    refused_with(
        &mut e,
        "INSERT INTO du_i VALUES (2, 0) ON CONFLICT (id) DO UPDATE SET k = 10",
        "duplicate key value violates unique constraint \"du_i_k_key\"",
    );
    assert_eq!(
        rows(&mut e, "SELECT id, k FROM du_i ORDER BY id"),
        "1:10,2:20"
    );
}

#[test]
fn a_text_unique_constraint_refuses_the_update_arm() {
    let mut e = setup(&[
        "CREATE TABLE du_t (id int PRIMARY KEY, k text UNIQUE)",
        "INSERT INTO du_t VALUES (1, 'a'), (2, 'b')",
    ]);
    refused_with(
        &mut e,
        "INSERT INTO du_t VALUES (2, 'z') ON CONFLICT (id) DO UPDATE SET k = 'a'",
        "du_t_k_key",
    );
    assert_eq!(
        rows(&mut e, "SELECT id, k FROM du_t ORDER BY id"),
        "1:a,2:b"
    );
}

/// Their `device_tokens.install_id`: the other key is a PARTIAL unique
/// index. PG names the index and gives the key as DETAIL.
#[test]
fn a_partial_unique_index_refuses_the_update_arm_in_postgresqls_words() {
    let mut e = setup(&[
        "CREATE TABLE du_p (id int PRIMARY KEY, tok text UNIQUE, install text)",
        "CREATE UNIQUE INDEX du_p_install ON du_p (install) WHERE install IS NOT NULL",
        "INSERT INTO du_p VALUES (1, 't1', 'i1'), (2, 't2', NULL)",
    ]);
    refused_with(
        &mut e,
        "INSERT INTO du_p VALUES (3, 't2', 'i1') ON CONFLICT (tok) DO UPDATE SET install = EXCLUDED.install",
        "duplicate key value violates unique constraint \"du_p_install\"",
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT id, tok, coalesce(install, 'NULL') FROM du_p ORDER BY id"
        ),
        "1:t1:i1,2:t2:NULL"
    );
}

/// The shapes that already answered: an upsert that changes nothing
/// another row holds, and a plain UPDATE into a held key.
#[test]
fn the_adjacent_shapes_still_answer() {
    let mut e = setup(&[
        "CREATE TABLE du_c (id int PRIMARY KEY, k int UNIQUE)",
        "INSERT INTO du_c VALUES (1, 10), (2, 20)",
    ]);
    e.execute("INSERT INTO du_c VALUES (2, 0) ON CONFLICT (id) DO UPDATE SET k = 30")
        .expect("a free key is fine");
    e.execute("INSERT INTO du_c VALUES (2, 0) ON CONFLICT (id) DO UPDATE SET k = 30")
        .expect("setting a row's key to the value it already holds is fine");
    refused_with(&mut e, "UPDATE du_c SET k = 10 WHERE id = 2", "du_c_k_key");
    assert_eq!(
        rows(&mut e, "SELECT id, k FROM du_c ORDER BY id"),
        "1:10,2:30"
    );
}

/// A plain UPDATE into a `CREATE UNIQUE INDEX` key used to answer
/// `UNIQUE INDEX "ux_k" violation on "ux": UPDATE of row #1 duplicates an
/// existing key`. PG's words, and a NULLS NOT DISTINCT index that the old
/// copy of the key logic let a second NULL into.
#[test]
fn an_update_into_a_unique_index_speaks_postgresqls_23505() {
    let mut e = setup(&[
        "CREATE TABLE ux (id int, k int)",
        "CREATE UNIQUE INDEX ux_k ON ux (k)",
        "INSERT INTO ux VALUES (1, 10), (2, 20)",
        "CREATE TABLE un (id int, k int)",
        "CREATE UNIQUE INDEX un_k ON un (k) NULLS NOT DISTINCT",
        "INSERT INTO un VALUES (1, NULL), (2, 20)",
    ]);
    refused_with(
        &mut e,
        "UPDATE ux SET k = 10 WHERE id = 2",
        "duplicate key value violates unique constraint \"ux_k\"",
    );
    refused_with(
        &mut e,
        "UPDATE un SET k = NULL WHERE id = 2",
        "duplicate key value violates unique constraint \"un_k\"",
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT id, coalesce(k::text, 'N') FROM un ORDER BY id"
        ),
        "1:N,2:20"
    );
}
