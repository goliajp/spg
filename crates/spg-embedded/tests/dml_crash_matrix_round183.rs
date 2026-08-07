//! v7.39 (read01 round 183) — embedded crash-recovery differential
//! matrix over DML shapes: the spg-embedded twin of the r179 server
//! matrix.
//!
//! r180 pinned three embedded shapes (autocommit RETURNING, writable
//! CTE, prepared RETURNING); this widens the net to the same 11-shape
//! surface the server matrix covers, plus prepared variants — one
//! database, every shape written, one simulated crash (`mem::forget`
//! skips Drop's checkpoint), one reopen, per-shape audits against the
//! pre-crash acked state. Any future embedded WAL-gate regression in
//! any shape fails by name.

use spg_embedded::Database;
use std::path::PathBuf;

struct Shape {
    name: &'static str,
    seed: &'static [&'static str],
    writes: &'static [&'static str],
    verify: &'static str,
    expect: usize,
}

const SHAPES: &[Shape] = &[
    Shape {
        name: "bare_insert",
        seed: &[],
        writes: &[
            "INSERT INTO {t} VALUES (1, 0)",
            "INSERT INTO {t} VALUES (2, 0)",
            "INSERT INTO {t} VALUES (3, 0)",
        ],
        verify: "SELECT id FROM {t}",
        expect: 3,
    },
    Shape {
        name: "insert_returning",
        seed: &[],
        writes: &[
            "INSERT INTO {t} VALUES (1, 0) RETURNING id",
            "INSERT INTO {t} VALUES (2, 0) RETURNING id",
        ],
        verify: "SELECT id FROM {t}",
        expect: 2,
    },
    Shape {
        name: "multirow_ins_ret",
        seed: &[],
        writes: &["INSERT INTO {t} VALUES (1, 0), (2, 0), (3, 0) RETURNING id"],
        verify: "SELECT id FROM {t}",
        expect: 3,
    },
    Shape {
        name: "tx_commit",
        seed: &[],
        writes: &[
            "BEGIN",
            "INSERT INTO {t} VALUES (1, 0)",
            "INSERT INTO {t} VALUES (2, 0)",
            "COMMIT",
        ],
        verify: "SELECT id FROM {t}",
        expect: 2,
    },
    Shape {
        name: "tx_returning",
        seed: &[],
        writes: &[
            "BEGIN",
            "INSERT INTO {t} VALUES (1, 0) RETURNING id",
            "INSERT INTO {t} VALUES (2, 0) RETURNING id",
            "COMMIT",
        ],
        verify: "SELECT id FROM {t}",
        expect: 2,
    },
    Shape {
        name: "update_returning",
        seed: &[
            "INSERT INTO {t} VALUES (1, 0)",
            "INSERT INTO {t} VALUES (2, 0)",
            "INSERT INTO {t} VALUES (3, 0)",
        ],
        writes: &["UPDATE {t} SET v = 9 WHERE id <= 2 RETURNING id"],
        verify: "SELECT id FROM {t} WHERE v = 9",
        expect: 2,
    },
    Shape {
        name: "delete_returning",
        seed: &[
            "INSERT INTO {t} VALUES (1, 0)",
            "INSERT INTO {t} VALUES (2, 0)",
            "INSERT INTO {t} VALUES (3, 0)",
        ],
        writes: &["DELETE FROM {t} WHERE id = 1 RETURNING id"],
        verify: "SELECT id FROM {t}",
        expect: 2,
    },
    Shape {
        name: "with_insert",
        seed: &[],
        writes: &["WITH s AS (SELECT 7 AS x) INSERT INTO {t} SELECT x, 0 FROM s"],
        verify: "SELECT id FROM {t}",
        expect: 1,
    },
    Shape {
        name: "with_ins_ret",
        seed: &[],
        writes: &["WITH s AS (SELECT 7 AS x) INSERT INTO {t} SELECT x, 0 FROM s RETURNING id"],
        verify: "SELECT id FROM {t}",
        expect: 1,
    },
    Shape {
        name: "merge",
        seed: &[
            "INSERT INTO {t} VALUES (1, 0)",
            "CREATE TABLE {t}_src (id BIGINT, v BIGINT)",
            "INSERT INTO {t}_src VALUES (1, 5)",
            "INSERT INTO {t}_src VALUES (2, 6)",
        ],
        writes: &["MERGE INTO {t} m USING {t}_src s ON m.id = s.id \
             WHEN MATCHED THEN UPDATE SET v = s.v \
             WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.v)"],
        verify: "SELECT id FROM {t}",
        expect: 2,
    },
    Shape {
        name: "merge_returning",
        seed: &[
            "INSERT INTO {t} VALUES (1, 0)",
            "CREATE TABLE {t}_src (id BIGINT, v BIGINT)",
            "INSERT INTO {t}_src VALUES (1, 5)",
            "INSERT INTO {t}_src VALUES (2, 6)",
        ],
        writes: &["MERGE INTO {t} m USING {t}_src s ON m.id = s.id \
             WHEN MATCHED THEN UPDATE SET v = s.v \
             WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.v) \
             RETURNING *"],
        verify: "SELECT id FROM {t}",
        expect: 2,
    },
];

fn subst(template: &str, table: &str) -> String {
    template.replace("{t}", table)
}

#[test]
fn embedded_dml_crash_matrix() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("spg-embed-matrix-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path: PathBuf = dir.join("spg.db");

    let mut audits: Vec<(String, String, usize)> = Vec::new();
    {
        let mut db = Database::open_path(&db_path).unwrap();
        for shape in SHAPES {
            let t = format!("m_{}", shape.name);
            let ctx = format!("embedded/{}", shape.name);
            db.execute(&format!("CREATE TABLE {t} (id BIGINT, v BIGINT)"))
                .unwrap();
            for s in shape.seed {
                db.execute(&subst(s, &t))
                    .unwrap_or_else(|e| panic!("[{ctx}] seed failed: {e}"));
            }
            for s in shape.writes {
                db.execute(&subst(s, &t))
                    .unwrap_or_else(|e| panic!("[{ctx}] write failed: {e}"));
            }
            audits.push((ctx, subst(shape.verify, &t), shape.expect));
        }
        // Prepared variants — bare + RETURNING.
        for (name, sql, expect) in [
            ("prep_bare", "INSERT INTO {t} VALUES ($1, 0)", 2usize),
            (
                "prep_returning",
                "INSERT INTO {t} VALUES ($1, 0) RETURNING id",
                2,
            ),
        ] {
            let t = format!("m_{name}");
            let ctx = format!("embedded-prep/{name}");
            db.execute(&format!("CREATE TABLE {t} (id BIGINT, v BIGINT)"))
                .unwrap();
            let stmt = db.prepare(&subst(sql, &t)).unwrap();
            for i in 1..=2_i64 {
                db.execute_prepared(&stmt, &[spg_storage::Value::BigInt(i)])
                    .unwrap_or_else(|e| panic!("[{ctx}] prepared write failed: {e}"));
            }
            audits.push((ctx, format!("SELECT id FROM {t}"), expect));
        }
        // Pre-crash audit: engine-visible state.
        for (ctx, verify, expect) in &audits {
            let got = db.query(verify).unwrap().len();
            assert_eq!(got, *expect, "[{ctx}] PRE-CRASH mismatch for {verify:?}");
        }
        std::mem::forget(db); // simulated crash — skip Drop's checkpoint
    }
    Database::force_unlock(&db_path).unwrap();

    let mut db = Database::open_path(&db_path).unwrap();
    let mut failures = Vec::new();
    for (ctx, verify, expect) in &audits {
        let got = db.query(verify).unwrap().len();
        if got != *expect {
            failures.push(format!("[{ctx}] {verify:?}: expected {expect}, got {got}"));
        }
    }
    assert!(
        failures.is_empty(),
        "embedded durability holes after crash+replay:\n{}",
        failures.join("\n")
    );
}
