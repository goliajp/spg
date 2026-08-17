//! r1054 (7.38 S3.1) — the live chain: dump → restore → diff.
//!
//! The dump's contract is SELF-consistency (design D27): restoring a
//! dump into a fresh engine and dumping again must produce the SAME
//! BYTES, and the restored data must checksum-match the original
//! through ordinary SQL. pg-emission fidelity is a separate,
//! registered campaign; a fixed point plus checksums is what makes
//! this dump trustworthy as a recovery artifact.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

/// The ironrules fixture generator's shape — nine types, two indexes,
/// deletes, updates — plus a view and an empty table.
fn rich_engine() -> Engine {
    let mut e = Engine::new();
    for sql in [
        "CREATE TABLE fx (id INT PRIMARY KEY, n NUMERIC, t TEXT NOT NULL, b BYTEA, \
         u UUID, d DATE, ts TIMESTAMPTZ, j JSONB, ia BIGINT[])",
        "INSERT INTO fx SELECT g, (g*7919%1000)::numeric/100, 'row-'||g, \
         decode(lpad(to_hex(g),8,'0'),'hex'), \
         ('00000000-0000-4000-8000-'||lpad(to_hex(g),12,'0'))::uuid, \
         '2026-01-01'::date + (g%365), \
         '2026-08-17 12:00:00+00'::timestamptz, \
         jsonb_build_object('k', g), ARRAY[g::bigint, (g*2)::bigint] \
         FROM generate_series(1, 200) g",
        "CREATE INDEX fx_n ON fx (n)",
        "CREATE INDEX fx_u ON fx (u)",
        "DELETE FROM fx WHERE id % 50 = 0",
        "UPDATE fx SET t = t || '-touched' WHERE id % 7 = 0",
        "CREATE TABLE fx_empty (a INT, b TEXT DEFAULT 'dflt')",
        "CREATE VIEW fx_view AS SELECT id, t FROM fx WHERE id < 10",
    ] {
        e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
    }
    e
}

fn checksum(e: &mut Engine) -> Vec<String> {
    [
        "SELECT count(*) FROM fx",
        "SELECT md5(string_agg(t, ',' ORDER BY id)) FROM fx",
        "SELECT md5(string_agg(n::text, ',' ORDER BY id)) FROM fx",
        "SELECT count(*) FROM fx_view",
        "SELECT count(*) FROM fx_empty",
    ]
    .iter()
    .map(|sql| one(e, sql))
    .collect()
}

#[test]
fn r1054_dump_restore_dump_is_a_fixed_point() {
    let mut a = rich_engine();
    let sum_a = checksum(&mut a);
    let dump1 = a.dump_sql().expect("dump 1");

    let mut b = Engine::new();
    for stmt in dump1.split(";\n").map(str::trim).filter(|s| !s.is_empty()) {
        b.execute(stmt)
            .unwrap_or_else(|err| panic!("restore failed on `{stmt}`: {err}"));
    }
    let sum_b = checksum(&mut b);
    assert_eq!(sum_a, sum_b, "restored data must checksum-match");

    let dump2 = b.dump_sql().expect("dump 2");
    if dump1 != dump2 {
        for (i, (l1, l2)) in dump1.lines().zip(dump2.lines()).enumerate() {
            if l1 != l2 {
                panic!("fixed point broken at line {i}:\n  dump1: {l1}\n  dump2: {l2}");
            }
        }
        panic!(
            "fixed point broken by length: dump1 {} lines, dump2 {} lines;\n tail1: {:?}\n tail2: {:?}",
            dump1.lines().count(),
            dump2.lines().count(),
            dump1.lines().last(),
            dump2.lines().last()
        );
    }
    // And the dump is substantial, not a vacuous empty match.
    assert!(dump1.matches("INSERT INTO").count() >= 2, "{}", dump1.len());
    assert!(dump1.contains("CREATE INDEX"), "secondary indexes present");
    assert!(dump1.contains("CREATE VIEW"), "views present");
}

/// The indexes must be REAL after restore — a seek must use them.
#[test]
fn r1054_restored_indexes_seek() {
    let mut a = rich_engine();
    let dump = a.dump_sql().unwrap();
    let mut b = Engine::new();
    for stmt in dump.split(";\n").map(str::trim).filter(|s| !s.is_empty()) {
        b.execute(stmt).unwrap();
    }
    let plan = match b
        .execute("EXPLAIN SELECT id FROM fx WHERE n = 1.23")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{other:?}"),
    };
    assert!(
        plan.contains("Index Scan") || plan.contains("Index Only Scan"),
        "restored fx_n must serve a seek: {plan}"
    );
}
