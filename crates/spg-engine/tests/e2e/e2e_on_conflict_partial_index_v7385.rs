//! 7.38.5 — an untargeted `ON CONFLICT` arbitrates on partial unique
//! indexes too.
//!
//! sentori r8, step 66. The bare form collected arbiters from every
//! unique constraint and every FULL unique index, but the filter that
//! built the index list excluded partial ones — so a conflict on a
//! partial unique index was invisible to `DO NOTHING` and escaped to
//! the duplicate-key check as an error. PG absorbs it.
//!
//! Theirs is an idempotency key: "one delivery per device per key",
//! enforced by a partial unique index and absorbed by an untargeted
//! DO NOTHING. Pressing send twice raised instead of being ignored —
//! a 500 to an operator who did the safe thing.
//!
//! The predicate decides which rows are in play, on BOTH sides of the
//! comparison: a row the predicate rejects is not in the index, so it
//! neither conflicts nor blocks anything else (differential-anchored
//! against PG18).

use spg_engine::{Engine, QueryResult};

fn affected(e: &mut Engine, sql: &str) -> usize {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::CommandOk { affected, .. } => affected,
        other => panic!("{sql}: {other:?}"),
    }
}

fn count(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn pin_v7385_untargeted_do_nothing_covers_a_partial_unique_index() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE oc (id INT, k TEXT, t TEXT)")
        .unwrap();
    e.execute("CREATE UNIQUE INDEX oc_plain ON oc (id)")
        .unwrap();
    e.execute("CREATE UNIQUE INDEX oc_partial ON oc (k, t) WHERE t IS NOT NULL")
        .unwrap();
    e.execute("INSERT INTO oc VALUES (1, 'a', 'x')").unwrap();

    // The plain index always absorbed its conflict.
    assert_eq!(
        affected(
            &mut e,
            "INSERT INTO oc VALUES (1, 'zz', 'yy') ON CONFLICT DO NOTHING"
        ),
        0
    );
    // The partial one raised. This is the whole defect.
    assert_eq!(
        affected(
            &mut e,
            "INSERT INTO oc VALUES (9, 'a', 'x') ON CONFLICT DO NOTHING"
        ),
        0
    );
    // Naming the target always worked, and must keep working.
    assert_eq!(
        affected(
            &mut e,
            "INSERT INTO oc VALUES (8, 'a', 'x') ON CONFLICT (k, t) WHERE t IS NOT NULL DO NOTHING"
        ),
        0
    );
    assert_eq!(count(&mut e, "SELECT count(*) FROM oc"), "1");
}

#[test]
fn pin_v7385_the_predicate_decides_who_is_in_play() {
    // A row the predicate rejects is not in the index: it does not
    // conflict, and it does not make later rows conflict either.
    // Measured against PG18 — both sides answer 3 rows.
    let mut e = Engine::new();
    e.execute("CREATE TABLE ocp (id INT, k TEXT, t TEXT)")
        .unwrap();
    e.execute("CREATE UNIQUE INDEX ocp_partial ON ocp (k) WHERE t IS NOT NULL")
        .unwrap();
    e.execute("INSERT INTO ocp VALUES (1, 'a', 'x')").unwrap();
    // Predicate false → outside the index → inserts, twice.
    assert_eq!(
        affected(
            &mut e,
            "INSERT INTO ocp VALUES (2, 'a', NULL) ON CONFLICT DO NOTHING"
        ),
        1
    );
    assert_eq!(
        affected(
            &mut e,
            "INSERT INTO ocp VALUES (3, 'a', NULL) ON CONFLICT DO NOTHING"
        ),
        1
    );
    // Predicate true → inside the index → absorbed.
    assert_eq!(
        affected(
            &mut e,
            "INSERT INTO ocp VALUES (4, 'a', 'y') ON CONFLICT DO NOTHING"
        ),
        0
    );
    assert_eq!(count(&mut e, "SELECT count(*) FROM ocp"), "3");
}

#[test]
fn pin_v7385_batch_local_keys_respect_the_predicate() {
    // The same rule inside ONE statement: two predicate-false rows with
    // the same key are both kept, because neither is in the index.
    let mut e = Engine::new();
    e.execute("CREATE TABLE ocb (id INT, k TEXT, t TEXT)")
        .unwrap();
    e.execute("CREATE UNIQUE INDEX ocb_partial ON ocb (k) WHERE t IS NOT NULL")
        .unwrap();
    assert_eq!(
        affected(
            &mut e,
            "INSERT INTO ocb VALUES (1, 'a', NULL), (2, 'a', NULL) ON CONFLICT DO NOTHING"
        ),
        2
    );
    // And two predicate-true rows with the same key: the second is
    // absorbed by the batch-local set.
    assert_eq!(
        affected(
            &mut e,
            "INSERT INTO ocb VALUES (3, 'b', 'x'), (4, 'b', 'x') ON CONFLICT DO NOTHING"
        ),
        1
    );
    assert_eq!(count(&mut e, "SELECT count(*) FROM ocb"), "3");
}
