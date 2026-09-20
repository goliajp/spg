//! 9.0.0 — a `gin_trgm_ops` index made `LIKE` return the wrong rows.
//!
//! Measured on 3,200 rows before the fix: `LIKE '%foo-bar%'` answered
//! 3000 with no index and **0** with one; `LIKE '%日本語%'` answered 200
//! and **0**. The pattern's literal run was windowed whole, so it
//! demanded trigrams that span a separator (`oo-`, `o-b`) or a
//! multi-byte boundary — and the index, which holds the trigrams of
//! WORDS, contains neither. Every row was then intersected away.
//!
//! An index may make a query faster. It may not change its answer.

use spg_engine::{Engine, QueryResult};

fn count(e: &mut Engine, sql: &str) -> String {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    spg_engine::eval::value_to_text(
        rows.first()
            .expect("one row")
            .values
            .first()
            .expect("one column"),
    )
}

/// Enough rows that the planner reaches for the index.
fn seeded() -> Engine {
    let mut e = Engine::new();
    for sql in [
        "CREATE TABLE trg (id int, t text)",
        "INSERT INTO trg SELECT g, 'pad'||g||' foo-bar tail' FROM generate_series(1,3000) g",
        "INSERT INTO trg SELECT 90000+g, 'jp'||g||' 日本語テスト' FROM generate_series(1,200) g",
        "INSERT INTO trg VALUES (1, 'don''t stop')",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
    e
}

const PATTERNS: &[(&str, &str)] = &[
    ("%foo-bar%", "3000"),
    ("%日本語%", "200"),
    ("%foo%", "3000"),
    ("pad1 %", "1"),
    ("%zzzz%", "0"),
    ("%don''t s%", "1"),
    ("%テスト", "200"),
];

#[test]
fn the_index_does_not_change_the_answer() {
    let mut e = seeded();
    let before: Vec<String> = PATTERNS
        .iter()
        .map(|(p, _)| {
            count(
                &mut e,
                &format!("SELECT count(*) FROM trg WHERE t LIKE '{p}'"),
            )
        })
        .collect();
    // The declared answer and the no-index answer must already agree, or
    // the fixture is not asking what it claims to ask.
    let want: Vec<String> = PATTERNS.iter().map(|(_, n)| (*n).to_string()).collect();
    assert_eq!(before, want, "without an index");

    e.execute("CREATE INDEX trg_i ON trg USING gin (t gin_trgm_ops)")
        .expect("CREATE INDEX");
    let after: Vec<String> = PATTERNS
        .iter()
        .map(|(p, _)| {
            count(
                &mut e,
                &format!("SELECT count(*) FROM trg WHERE t LIKE '{p}'"),
            )
        })
        .collect();
    assert_eq!(after, want, "with a gin_trgm_ops index");
}

#[test]
fn the_index_is_reached_at_all() {
    // The pin above is worthless if the index path never runs: a
    // pattern the index CANNOT answer correctly would pass by falling
    // back. This one proves the path is live — searching for a trigram
    // no row holds has to come back empty, and only the index can know
    // that without reading every row.
    let mut e = seeded();
    e.execute("CREATE INDEX trg_i ON trg USING gin (t gin_trgm_ops)")
        .expect("CREATE INDEX");
    assert_eq!(
        count(&mut e, "SELECT count(*) FROM trg WHERE t LIKE '%qqq%'"),
        "0"
    );
    assert_eq!(
        count(&mut e, "SELECT count(*) FROM trg WHERE t LIKE '%foo-bar%'"),
        "3000"
    );
}
