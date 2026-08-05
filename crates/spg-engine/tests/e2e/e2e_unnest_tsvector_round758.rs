//! Round 758 (F31-B8a) — `unnest(tsvector)` in FROM: one row per
//! lexeme with PG's columns lexeme | positions | weights,
//! PG18-measured (`a | {1,3} | {D,D}`; WITH ORDINALITY appends the
//! counter; a position-less lexeme reads NULL in both array columns).
//! SPG refused the whole form ("unnest() expects an array argument,
//! got tsvector") — the round-753 audit probe's original shape.
//! The lateral half of that probe (`unnest(t.positions)` referencing
//! the first unnest) is F31-B8b, still queued.

use spg_engine::{Engine, QueryResult};

fn grid(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, columns } => {
            let mut out = vec![columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>().join("|")];
            out.extend(rows.iter().map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            }));
            out
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn round758_unnest_tsvector_answers_as_pg() {
    let mut e = Engine::new();
    assert_eq!(
        grid(&mut e, "SELECT * FROM unnest(to_tsvector('simple','a b a'))"),
        [
            "lexeme|positions|weights",
            "a|{1,3}|{D,D}",
            "b|{2}|{D}",
        ]
    );
    // Columns are addressable by PG's names, qualified or not.
    assert_eq!(
        grid(
            &mut e,
            "SELECT lexeme, positions FROM unnest(to_tsvector('simple','b a')) \
             WHERE lexeme = 'a'"
        ),
        ["lexeme|positions", "a|{2}"]
    );
    assert_eq!(
        grid(
            &mut e,
            "SELECT t.lexeme FROM unnest(to_tsvector('simple','x y')) t ORDER BY 1"
        ),
        ["lexeme", "x", "y"]
    );
    // WITH ORDINALITY rides on top.
    assert_eq!(
        grid(
            &mut e,
            "SELECT * FROM unnest(to_tsvector('simple','a')) WITH ORDINALITY"
        ),
        ["lexeme|positions|weights|ordinality", "a|{1}|{D}|1"]
    );
}
