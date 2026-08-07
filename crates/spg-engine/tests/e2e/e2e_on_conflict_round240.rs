//! v7.39 (round 240) — the ON CONFLICT surface, swept 20 cases against
//! live PG18.4 (2026-07-19). The DO UPDATE core (EXCLUDED.*, t.col
//! references, WHERE on the conflict row, ON CONSTRAINT, RETURNING)
//! already matched; the gaps:
//!
//!   * a BARE `ON CONFLICT DO NOTHING` arbitrated on ONE constraint (the
//!     first), so a row conflicting on any other raised a duplicate-key
//!     error straight through the clause;
//!   * an explicit target no unique constraint enforces stays ACCEPTED —
//!     PG refuses it (42P10), but mailrs's caldav upsert model relies on
//!     the lax form, so the divergence is deliberate and locked below;
//!   * bare `ON CONFLICT DO UPDATE` must name its target (PG 42601);
//!   * `ON CONFLICT (col) WHERE pred DO …` (the partial-index inference
//!     clause) did not parse;
//!   * `INSERT INTO t AS alias` did not parse, and the alias is what DO
//!     UPDATE refers to the target row by;
//!   * touching the same row twice in one command leaked the scalar
//!     subquery's cardinality message instead of PG's.

use spg_engine::{Engine, QueryResult};

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int PRIMARY KEY, v int, tag text UNIQUE)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1,10,'a'),(2,20,'b')")
        .unwrap();
    e
}

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(ok) => panic!("{sql}: expected an error, got {ok:?}"),
    }
}

#[test]
fn bare_on_conflict_arbitrates_on_every_constraint() {
    let mut e = seeded();
    // Conflicts on the PK — used to escalate to a duplicate-key error
    // because the arbiter picked was the tag constraint.
    e.execute("INSERT INTO t VALUES (1,99,'z') ON CONFLICT DO NOTHING")
        .unwrap();
    // Conflicts on the UNIQUE tag.
    e.execute("INSERT INTO t VALUES (9,9,'a') ON CONFLICT DO NOTHING")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM t"), "2");
    // A table with no unique anything still accepts the bare form — no
    // arbiter simply means no conflict is possible (probed).
    e.execute("CREATE TABLE nu (a int)").unwrap();
    e.execute("INSERT INTO nu VALUES (1) ON CONFLICT DO NOTHING")
        .unwrap();
    e.execute("INSERT INTO nu VALUES (1) ON CONFLICT DO NOTHING")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM nu"), "2");
}

#[test]
fn conflict_target_must_match_a_unique_constraint() {
    let mut e = seeded();
    // DELIBERATE divergence, locked as such: PG refuses a target no unique
    // constraint enforces (42P10); SPG accepts any column list and
    // arbitrates on it — mailrs's caldav upsert model
    // (`ON CONFLICT (uid, calendar_id)` with no declared constraint) has
    // always run on the lax form, and zero-customer-change outranks the
    // alignment. The laxness only ACCEPTS more: a PG-valid program never
    // issues the shape PG rejects.
    e.execute("INSERT INTO t VALUES (9,9,'x') ON CONFLICT (v) DO NOTHING")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM t"), "3");
    // The enforced targets still work, in both spellings.
    e.execute("INSERT INTO t VALUES (1,50,'z') ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT v FROM t WHERE id = 1"), "50");
    e.execute("INSERT INTO t VALUES (9,9,'a') ON CONFLICT (tag) DO UPDATE SET v = 111")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT v FROM t WHERE tag = 'a'"), "111");
}

#[test]
fn bare_do_update_requires_a_target() {
    let mut e = seeded();
    let got = err(
        &mut e,
        "INSERT INTO t VALUES (1,1,'q') ON CONFLICT DO UPDATE SET v = 0",
    );
    assert!(
        got.contains("ON CONFLICT DO UPDATE requires inference specification or constraint name"),
        "{got}"
    );
}

#[test]
fn index_predicate_parses_and_alias_binds_the_target_row() {
    let mut e = seeded();
    // The partial-index inference clause: a full unique index satisfies
    // any predicate, so this is a no-op skip (probed against PG).
    e.execute("INSERT INTO t VALUES (1,6,'h') ON CONFLICT (id) WHERE v > 0 DO NOTHING")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT v FROM t WHERE id = 1"), "10");
    // `INSERT INTO t AS me` — the alias is how DO UPDATE reads the
    // existing row.
    e.execute(
        "INSERT INTO t AS me (id,v) VALUES (1,1) ON CONFLICT (id) DO UPDATE SET v = me.v + 1",
    )
    .unwrap();
    assert_eq!(one(&mut e, "SELECT v FROM t WHERE id = 1"), "11");
}

#[test]
fn touching_a_row_twice_uses_pgs_wording() {
    let mut e = seeded();
    let got = err(
        &mut e,
        "INSERT INTO t VALUES (4,4,'d'),(4,5,'e') ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v",
    );
    assert!(
        got.contains("ON CONFLICT DO UPDATE command cannot affect row a second time"),
        "{got}"
    );
    // The old message was the scalar subquery's — an internal leak here.
    assert!(!got.contains("subquery"), "{got}");
}
