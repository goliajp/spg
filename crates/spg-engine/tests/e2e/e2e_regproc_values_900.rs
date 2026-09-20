//! 9.0.0 — the `regproc` columns carry the TYPE but not the VALUE.
//!
//! `pg_type.typinput` read `-` — oid 0, PostgreSQL's own spelling for
//! "no function" — where PG 18.6 names `int4in`, and every `pg_am`
//! row's `amhandler` read `-` where PG names `bthandler`. SPG's I/O is
//! built into the engine and had no catalogued function to point at, so
//! naming one would have left `pg_type JOIN pg_proc ON p.oid =
//! typinput` dangling; the functions are in `pg_proc` now, at
//! PostgreSQL's own oids, and the join resolves.
//!
//! All 101 types the two engines share match on all four I/O columns,
//! measured. Three pseudo-types the I/O functions RETURN had to join
//! the catalog for the same reason: `cstring`, `table_am_handler` and
//! `index_am_handler`, without which 64 `pg_proc` rows pointed at a
//! type nothing carried.
//!
//! The join also needed the key encoding widened: a reg value carried
//! its own tag into the join key and never met an `oid` column's
//! integer, so `pg_type JOIN pg_proc ON p.oid = t.typinput` found
//! NOTHING while `(SELECT typinput …) = (SELECT oid …)` answered `t`.
//! Same defect as the numeric-width one the encoder was widened for.

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
fn a_types_io_functions_are_named() {
    let mut e = Engine::new();
    assert_eq!(
        col(
            &mut e,
            "SELECT typinput::text||' '||typoutput::text||' '||typreceive::text||' '\
             ||typsend::text FROM pg_type WHERE typname='int4'"
        ),
        vec!["int4in int4out int4recv int4send".to_string()]
    );
    // A pseudo-type has no binary I/O on PostgreSQL either.
    assert_eq!(
        col(
            &mut e,
            "SELECT typreceive::text FROM pg_type WHERE typname='table_am_handler'"
        ),
        vec!["-".to_string()]
    );
}

#[test]
fn an_access_method_names_its_handler() {
    let mut e = Engine::new();
    assert_eq!(
        col(&mut e, "SELECT amhandler::text FROM pg_am ORDER BY amname"),
        vec![
            "brinhandler".to_string(),
            "bthandler".to_string(),
            "ginhandler".to_string(),
            "gisthandler".to_string(),
            "hashhandler".to_string(),
            "heap_tableam_handler".to_string(),
            "spghandler".to_string(),
        ]
    );
}

#[test]
fn the_join_resolves_and_nothing_dangles() {
    let mut e = Engine::new();
    // Every named I/O function is a pg_proc row…
    assert_eq!(
        col(
            &mut e,
            "SELECT count(*) FROM pg_type t WHERE t.typinput <> 0 \
             AND NOT EXISTS (SELECT 1 FROM pg_proc p WHERE p.oid = t.typinput)"
        ),
        vec!["0".to_string()]
    );
    // …and the join itself finds them, which is the half the key
    // encoding used to lose.
    let joined = col(
        &mut e,
        "SELECT count(*) FROM pg_type t JOIN pg_proc p ON p.oid = t.typinput",
    );
    let named = col(&mut e, "SELECT count(*) FROM pg_type WHERE typinput <> 0");
    assert_eq!(joined, named);
    assert_ne!(joined, vec!["0".to_string()]);
    // Every access method's handler likewise.
    assert_eq!(
        col(
            &mut e,
            "SELECT count(*) FROM pg_am a JOIN pg_proc p ON p.oid = a.amhandler"
        ),
        vec!["7".to_string()]
    );
}

#[test]
fn no_pg_proc_row_points_at_a_type_nothing_carries() {
    let mut e = Engine::new();
    assert_eq!(
        col(
            &mut e,
            "SELECT count(*) FROM pg_proc p \
             WHERE NOT EXISTS (SELECT 1 FROM pg_type t WHERE t.oid = p.prorettype)"
        ),
        vec!["0".to_string()]
    );
}
