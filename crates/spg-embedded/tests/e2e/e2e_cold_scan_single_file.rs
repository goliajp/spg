//! v7.34.1 (mailrs prod report bug A) — single-file catalogs whose
//! filename already ends in `.spg` must not noise-log the cold-segment
//! scan.
//!
//! The cold-tier scan computes `<parent>/<stem>.spg/segments` from the
//! db_path. When the db_path itself is `<parent>/<stem>.spg` (mailrs's
//! `mailrs.spg` shape), the cold dir resolves to `<file>/segments` —
//! traversing through a regular file inode, which the kernel rejects
//! with ENOTDIR. Pre-7.34.1 boot logged `cold-segment scan: cannot
//! read ...: Not a directory (os error 20)` on every start. Post-fix
//! the scan checks `is_dir()` first and returns silently when there's
//! no real segments directory to walk.
//!
//! The assertion here is correctness-only: a `.spg`-named catalog
//! round-trips a write across a reopen. Capturing the absence of an
//! eprintln in tests is awkward, but this exercises the boot path
//! that previously hit the ENOTDIR branch.

use std::path::PathBuf;

use spg_embedded::Database;
use spg_storage::Value;

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-embed-cold-single-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn dot_spg_filename_does_not_break_reopen() {
    // Reproduce the mailrs filename shape exactly: the catalog is a
    // regular file whose name ends in `.spg`.
    let dir = unique_tmpdir("dot-spg");
    let db_path = dir.join("mailrs.spg");
    {
        let mut db = Database::open_path(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
            .unwrap();
        for i in 1..=5 {
            db.execute(&format!("INSERT INTO t VALUES ({i}, 'r-{i}')"))
                .unwrap();
        }
    }
    // The "segments" subpath here is `<dir>/mailrs.spg/segments` —
    // traversing through the regular file. Pre-fix this triggered
    // ENOTDIR; post-fix it's a quiet no-op.
    assert!(
        db_path.is_file(),
        "preconditions: catalog is a regular file, not a directory"
    );
    let bogus_segments = db_path.join("segments");
    assert!(
        !bogus_segments.is_dir(),
        "preconditions: the path the scan would walk is a non-directory"
    );

    let mut db = Database::open_path(&db_path).unwrap();
    let rows = db.query("SELECT COUNT(*) FROM t").unwrap();
    match &rows[0][0] {
        Value::BigInt(5) => {}
        Value::Int(5) => {}
        other => panic!("expected 5 rows, got {other:?}"),
    }
}
