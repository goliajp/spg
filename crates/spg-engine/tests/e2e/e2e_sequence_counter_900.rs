//! 9.0.0 — a serial column drew from the table's MAX, not from a counter.
//!
//! PostgreSQL's `nextval` does not consult the table, so an explicit id
//! does not move it. Measured on 18.6:
//!
//! ```text
//!   INSERT (v);  INSERT (id=50,v);  INSERT (v);  INSERT (v)
//!     PG   1, 50, 2, 3          SPG  1, 50, 51, 52
//! ```
//!
//! SPG derived each value from `max(id) + 1`, which is MySQL's
//! AUTO_INCREMENT rule and not PostgreSQL's. The counter lives in the
//! column's implicit sequence, so the sequence has to exist: it is born
//! at the first insert that needs it, seeded from the table's current
//! max so no value the table already holds is handed out twice. That
//! one-time catch-up is what keeps an existing database from colliding
//! on the upgrade.
//!
//! The MySQL dialect keeps its own rule — measured on 9.7.2, an
//! explicit 50 moves AUTO_INCREMENT to 51 there.
//!
//! Two defects this uncovered, both masked by the max-based rule:
//! a fresh implicit sequence answered 0 rather than its START of 1, and
//! `OVERRIDING USER VALUE` replaced the value written on a plain SERIAL
//! column, where PostgreSQL keeps it (it applies to identity columns).

use spg_engine::{Engine, QueryResult};

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    rows.into_iter()
        .map(|r| spg_engine::eval::value_to_text(r.values.first().expect("one column")))
        .collect()
}

#[test]
fn an_explicit_id_does_not_move_the_counter() {
    let mut e = Engine::new();
    for sql in [
        "CREATE TABLE c7 (id serial PRIMARY KEY, v text)",
        "INSERT INTO c7(v) VALUES ('a')",
        "INSERT INTO c7(id,v) VALUES (50,'b')",
        "INSERT INTO c7(v) VALUES ('c')",
        "INSERT INTO c7(v) VALUES ('d')",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
    assert_eq!(
        col(&mut e, "SELECT id||':'||v FROM c7 ORDER BY id"),
        vec![
            "1:a".to_string(),
            "2:c".to_string(),
            "3:d".to_string(),
            "50:b".to_string()
        ]
    );
}

#[test]
fn the_counter_starts_at_one_and_catches_up_once() {
    let mut e = Engine::new();
    // A table whose rows arrived with explicit ids: the sequence is born
    // at the first generated insert and must not hand out a value the
    // table already holds. PG answers 1 here too, because its sequence
    // was never advanced.
    for sql in [
        "CREATE TABLE c7b (id serial PRIMARY KEY, v text)",
        "INSERT INTO c7b(id,v) VALUES (10,'x'),(20,'y')",
        "INSERT INTO c7b(v) VALUES ('z')",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
    assert_eq!(
        col(&mut e, "SELECT id||':'||v FROM c7b ORDER BY id"),
        vec!["1:z".to_string(), "10:x".to_string(), "20:y".to_string()]
    );
}

#[test]
fn an_insert_consumes_from_the_sequence() {
    // The INSERT and `nextval` draw from the same counter: measured on
    // PG 18.6, setval(24304) then an INSERT then nextval answers 24305
    // then 24306.
    let mut e = Engine::new();
    e.execute("CREATE TABLE c7c (id serial PRIMARY KEY, v text)")
        .expect("create");
    e.execute("SELECT setval('c7c_id_seq', 24304, true)")
        .expect("setval");
    e.execute("INSERT INTO c7c(v) VALUES ('new')")
        .expect("insert");
    assert_eq!(col(&mut e, "SELECT max(id) FROM c7c"), vec!["24305"]);
    assert_eq!(col(&mut e, "SELECT nextval('c7c_id_seq')"), vec!["24306"]);
}

#[test]
fn overriding_user_value_leaves_a_plain_serial_alone() {
    // Measured on PG 18.6: OVERRIDING USER VALUE applies to an IDENTITY
    // column; a plain SERIAL keeps the value written.
    let mut e = Engine::new();
    e.execute("CREATE TABLE o (id SERIAL, v INT)")
        .expect("create");
    e.execute("INSERT INTO o (id, v) OVERRIDING USER VALUE VALUES (43, 2)")
        .expect("insert");
    assert_eq!(col(&mut e, "SELECT id FROM o"), vec!["43"]);
}
