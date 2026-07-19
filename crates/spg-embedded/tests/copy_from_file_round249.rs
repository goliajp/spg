//! v7.39 (round 249) — the `COPY … FROM '<file>'` HOST endpoint on the
//! embedded database, differentially probed against live PG18.4
//! (2026-07-19). The no_std engine performs no I/O, so
//! `Database::execute` reads the file itself and lowers to per-row
//! INSERTs through the normal execute path: every row gets its WAL
//! record inside one wrapping transaction (one atomic commit, one
//! fsync), which is also PG's all-or-nothing COPY. The r180 lesson —
//! a write path that skips the WAL bookkeeping loses data silently —
//! is why the reopen assertions below exist.

use spg_embedded::Database;

/// v7.39 (round 258) — the four tests in this file run in PARALLEL and
/// each removes its directory at the end, so the name has to be unique
/// per TEST, not merely per instant: `SystemTime::now()` is not
/// nanosecond-distinct on macOS, two tests could land on the same
/// directory, and one's cleanup deleted the other's fixture files. Seen
/// as a flake in `a_bad_row_leaves_nothing_even_across_reopen` ("could
/// not open file … bad.csv"). A per-test tag plus a counter removes the
/// collision entirely.
fn tmp(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let p = std::env::temp_dir().join(format!(
        "spg-copy-from-{tag}-{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn count(db: &mut Database, sql: &str) -> i64 {
    match db.execute(sql).unwrap() {
        spg_engine::QueryResult::Rows { rows, .. } => match rows[0].values[0] {
            spg_storage::Value::BigInt(n) => n,
            ref other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn copy_from_file_lands_and_survives_reopen() {
    let dir = tmp("lands-reopen");
    let path = dir.join("d.db");
    let csv = dir.join("in.csv");
    std::fs::write(&csv, "id,name,v\n1,a,10\n2,\"b,comma\",\n3,c,30\n").unwrap();
    {
        let mut db = Database::open_path(&path).unwrap();
        db.execute("CREATE TABLE ct (id int, name text, v int)").unwrap();
        let r = db
            .execute(&format!(
                "COPY ct FROM '{}' WITH (FORMAT csv, HEADER)",
                csv.display()
            ))
            .unwrap();
        assert!(
            matches!(r, spg_engine::QueryResult::CommandOk { affected: 3, .. }),
            "{r:?}"
        );
        assert_eq!(count(&mut db, "SELECT count(*) FROM ct"), 3);
    } // Drop = clean shutdown flush.
    {
        let mut db = Database::open_path(&path).unwrap();
        assert_eq!(count(&mut db, "SELECT count(*) FROM ct"), 3);
        match db.execute("SELECT name FROM ct WHERE id = 2").unwrap() {
            spg_engine::QueryResult::Rows { rows, .. } => {
                assert_eq!(rows[0].values[0], spg_storage::Value::Text("b,comma".into()));
            }
            other => panic!("{other:?}"),
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_bad_row_leaves_nothing_even_across_reopen() {
    let dir = tmp("bad-row");
    let path = dir.join("d.db");
    let csv = dir.join("bad.csv");
    std::fs::write(&csv, "1,a,10\n2,b,notanint\n").unwrap();
    {
        let mut db = Database::open_path(&path).unwrap();
        db.execute("CREATE TABLE ct (id int, name text, v int)").unwrap();
        let got = format!(
            "{}",
            db.execute(&format!("COPY ct FROM '{}' WITH (FORMAT csv)", csv.display()))
                .unwrap_err()
        );
        assert!(
            got.contains("invalid input syntax for type integer: \"notanint\""),
            "{got}"
        );
        // PG: all-or-nothing — row 1 must not survive the failed COPY.
        assert_eq!(count(&mut db, "SELECT count(*) FROM ct"), 0);
    }
    {
        let mut db = Database::open_path(&path).unwrap();
        // Nor may WAL replay resurrect it (the rolled-back transaction
        // writes no commit record).
        assert_eq!(count(&mut db, "SELECT count(*) FROM ct"), 0);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_file_and_missing_relation_take_pgs_wordings() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE ct (id int)").unwrap();
    // The relation check runs BEFORE the file is opened (PG's order):
    // a missing table on a missing file names the relation.
    let got = format!(
        "{}",
        db.execute("COPY nope FROM '/definitely/not/here.csv'").unwrap_err()
    );
    assert!(got.contains("relation \"nope\" does not exist"), "{got}");
    // A missing file on a real table: PG's wording, no os-error suffix.
    let got = format!(
        "{}",
        db.execute("COPY ct FROM '/definitely/not/here.csv'").unwrap_err()
    );
    assert!(
        got.contains("could not open file \"/definitely/not/here.csv\" for reading:"),
        "{got}"
    );
    assert!(!got.contains("os error"), "{got}");
}

#[test]
fn text_format_and_explicit_columns_work_on_the_file_endpoint() {
    let dir = tmp("text-cols");
    let txt = dir.join("in.txt");
    std::fs::write(&txt, "1\tx\n2\t\\N\n").unwrap();
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE ct (id int, name text, v int DEFAULT 7)").unwrap();
    db.execute(&format!("COPY ct (id, name) FROM '{}'", txt.display())).unwrap();
    match db.execute("SELECT id, name, v FROM ct ORDER BY id").unwrap() {
        spg_engine::QueryResult::Rows { rows, .. } => {
            let render: Vec<String> = rows
                .iter()
                .map(|r| {
                    r.values
                        .iter()
                        .map(spg_engine::eval::value_to_text)
                        .collect::<Vec<_>>()
                        .join("|")
                })
                .collect();
            // PG fills omitted columns with their defaults (probed:
            // psql -tA prints 1|x|7 and 2||7; value_to_text spells the
            // NULL out).
            assert_eq!(render, ["1|x|7", "2|NULL|7"]);
        }
        other => panic!("{other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// v7.39 (round 252) — the `COPY … TO '<file>'` HOST endpoint: the
/// engine renders (read-only), the embedded host writes the file.
/// Payloads probed byte-identical to PG's written files.
#[test]
fn copy_to_file_writes_pg_identical_bytes() {
    let dir = tmp("copy-to");
    let out = dir.join("out.csv");
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE ct (id int, name text)").unwrap();
    db.execute("INSERT INTO ct VALUES (1,'a'),(2,NULL)").unwrap();
    let r = db
        .execute(&format!(
            "COPY ct TO '{}' WITH (FORMAT csv, HEADER)",
            out.display()
        ))
        .unwrap();
    // `COPY 2` — the header line is payload, not count.
    assert!(
        matches!(r, spg_engine::QueryResult::CommandOk { affected: 2, .. }),
        "{r:?}"
    );
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "id,name\n1,a\n2,\n");
    // Overwrite works (PG semantics).
    db.execute(&format!("COPY ct (id) TO '{}'", out.display())).unwrap();
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "1\n2\n");
    // Unwritable path takes PG's wording, no os-error suffix.
    let got = format!(
        "{}",
        db.execute("COPY ct TO '/definitely/not/here/out.csv'").unwrap_err()
    );
    assert!(
        got.contains("could not open file \"/definitely/not/here/out.csv\" for writing:"),
        "{got}"
    );
    assert!(!got.contains("os error"), "{got}");
    let _ = std::fs::remove_dir_all(&dir);
}
