//! 7.38.7 — a jsonb column survives a checkpoint.
//!
//! The schema-driven encoder treated an unmatched (value tag, column
//! type) pair as `unreachable!()`, on the reasoning that `Table::insert`
//! had already validated it. It had — under a looser rule than the
//! encoder's — and a TEXT body in a jsonb column killed the CHECKPOINT
//! THREAD on sentori's real data. That is the worst way a durability
//! path can fail: writes keep being acknowledged while nothing reaches
//! disk, and the only symptom is a thread that is no longer there.
//!
//! Found by pointing the dogfood replay at their dump; it took seconds.
//! This is the same failure in the smallest form that reproduces it, and
//! it lives here because only this crate has the file-backed database
//! that makes the encoder run at all.

use spg_embedded::Database;
use spg_engine::QueryResult;

fn count(db: &mut Database, sql: &str) -> String {
    match db.execute(sql).expect(sql) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn v7387_jsonb_survives_checkpoint_and_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("t.db");
    {
        let mut db = Database::open_path(&path).expect("open");
        db.execute("CREATE TABLE j (id INT, payload JSONB)")
            .unwrap();
        // Every spelling — a bare literal, an explicit ::jsonb, and the
        // COPY form the dump uses — because all three reach the encoder
        // through the same column.
        db.execute("INSERT INTO j VALUES (1, '{\"a\":1}')").unwrap();
        db.execute("INSERT INTO j VALUES (2, '{\"b\":2}'::jsonb)")
            .unwrap();
        db.execute_script("COPY j (id, payload) FROM stdin;\n3\t{\"c\": 3}\n\\.\n")
            .unwrap();
        assert_eq!(count(&mut db, "SELECT count(*) FROM j"), "3");
    }
    // The reopen is the assertion: if the checkpoint thread died, the
    // rows are not here.
    let mut db = Database::open_path(&path).expect("reopen");
    assert_eq!(
        count(&mut db, "SELECT count(*) FROM j"),
        "3",
        "jsonb rows did not survive a checkpoint and reopen — the encoder \
         is refusing a pair the insert path accepted"
    );
    assert_eq!(
        count(
            &mut db,
            "SELECT count(*) FROM j WHERE payload @> '{\"a\":1}'::jsonb"
        ),
        "1",
        "the jsonb VALUE did not round-trip, only the row"
    );
}
