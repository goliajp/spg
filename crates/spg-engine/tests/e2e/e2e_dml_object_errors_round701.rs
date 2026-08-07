//! Round 701 — the DML/DDL batch, and the class fix behind two rounds of
//! one-at-a-time patches.
//!
//! Ten shapes measured against PG18; eight already agreed. The two that did
//! not were both about the SENTENCE, not the refusal:
//!
//!   * `COPY nosuch TO STDOUT` reached the client as `storage: relation
//!     "nosuch" does not exist`. `storage: ` names a layer inside SPG.
//!   * `CREATE INDEX ix ON t(nope)` said `column not found: nope`, where PG
//!     says `column "nope" does not exist`.
//!
//! The second is a straight miss: `EvalError::ColumnNotFound` was changed to
//! PG's wording in read01 round 81, for the exact reason that the old
//! spelling "matches none of the wire layer's `does not exist` patterns".
//! `StorageError::ColumnNotFound` was not, so which sentence a caller got
//! depended on which layer noticed the missing column.
//!
//! The first is the third instance of one thing, and the reason this round
//! stopped patching instances. pgwire strips SPG's internal prefixes only
//! when the SQLSTATE is not the generic 42000 — written for `unsupported: `,
//! which really is meaningful in that class, and it happened to gate the
//! whole peel. So every unclassified error kept whatever layer name it had:
//! round 698 met `corrupt on-disk format: ` on a sequence, round 700 met it
//! again on a trigger, and this one met `storage: ` on a COPY. Only
//! `unsupported: ` is class-dependent now; the layer names come off in every
//! class, because they are never PG's vocabulary in any of them.
//!
//! These pins are engine-side, so they check the message SPG produces. The
//! prefix itself is a wire concern and is pinned in pgwire's own tests.

use spg_engine::Engine;

fn err_of(e: &mut Engine, sql: &str) -> String {
    format!(
        "{}",
        e.execute(sql).expect_err(&format!("PG18 refuses: {sql}"))
    )
}

/// PG's wording for a missing column, whichever layer notices it.
#[test]
fn round701_a_missing_column_reads_the_same_from_either_layer() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t701(i INT)").unwrap();
    // Storage notices this one (CREATE INDEX resolves against the schema).
    let ddl = err_of(&mut e, "CREATE INDEX ix701 ON t701(nosuch701)");
    assert!(ddl.contains("column \"nosuch701\" does not exist"), "{ddl}");
    assert!(!ddl.contains("column not found"), "the old spelling: {ddl}");
    // Eval notices this one. Same sentence.
    let dml = err_of(&mut e, "SELECT nosuch701 FROM t701");
    assert!(dml.contains("column \"nosuch701\" does not exist"), "{dml}");
}

/// Every shape in the batch that names a missing relation says so, with no
/// layer name in front.
#[test]
fn round701_a_missing_relation_says_only_that() {
    let mut e = Engine::new();
    for sql in [
        "COPY nosuch701 TO STDOUT",
        "TRUNCATE nosuch701",
        "CREATE TABLE c701 (LIKE nosuch701)",
        "CREATE TABLE d701 (i INT) INHERITS (nosuch701)",
        "INSERT INTO nosuch701 VALUES (1)",
        "UPDATE nosuch701 SET i = 1",
        "DELETE FROM nosuch701",
    ] {
        let err = err_of(&mut e, sql);
        assert!(
            err.contains("relation \"nosuch701\" does not exist"),
            "{sql}\n  got: {err}"
        );
    }
}

/// The two duplicate-name shapes, which already matched, pinned with the
/// rest so a later change answers for the batch.
#[test]
fn round701_the_duplicate_name_shapes() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t701(i INT)").unwrap();
    assert!(
        err_of(&mut e, "ALTER TABLE t701 ADD COLUMN i INT")
            .contains("column \"i\" of relation \"t701\" already exists")
    );
    assert!(
        err_of(&mut e, "CREATE TABLE t701(i INT)").contains("relation \"t701\" already exists")
    );
}
