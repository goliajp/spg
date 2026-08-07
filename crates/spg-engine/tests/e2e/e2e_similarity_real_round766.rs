//! Round 766 (F31 tranche 4 #107) — pg_trgm's similarity() returns
//! REAL, PG18-measured (`0.33333334 | real`); SPG answered the f64
//! (16 digits, double precision) under a comment that knew better.

use spg_engine::{Engine, QueryResult};

#[test]
fn round766_similarity_returns_real() {
    let mut e = Engine::new();
    let QueryResult::Rows { rows, .. } = e
        .execute("SELECT similarity('abc','abd'), pg_typeof(similarity('abc','abd'))")
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        spg_engine::eval::value_to_text(&rows[0].values[0]),
        "0.33333334"
    );
    assert_eq!(spg_engine::eval::value_to_text(&rows[0].values[1]), "real");
}
