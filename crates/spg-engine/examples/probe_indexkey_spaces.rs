//! r1039 — do NUMERIC and BYTEA columns actually take the index now, and
//! do they answer the same as a scan?
//!
//! Two questions, and the second is the one that matters: an index that
//! is used but disagrees with a scan is worse than one that is not used.
use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        Ok(o) => vec![format!("{o:?}")],
        Err(err) => vec![format!("ERR {err}")],
    }
}

fn build(ty: &str, vals: &[String], indexed: bool) -> Engine {
    let mut e = Engine::new();
    e.execute(&format!("CREATE TABLE t (id INT PRIMARY KEY, v {ty})"))
        .unwrap();
    for (i, v) in vals.iter().enumerate() {
        e.execute(&format!("INSERT INTO t VALUES ({}, {v})", i + 1))
            .unwrap();
    }
    if indexed {
        e.execute("CREATE INDEX tv ON t (v)").unwrap();
    }
    e
}

fn main() {
    // NUMERIC: the same value at several scales, negatives, zero, and the
    // three specials.
    let nums: Vec<String> = [
        "1.5",
        "1.50",
        "1.500",
        "2",
        "2.0",
        "-1.5",
        "-2",
        "0",
        "0.0",
        "0.001",
        "1000000",
        "'NaN'::numeric",
        "'Infinity'::numeric",
        "'-Infinity'::numeric",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    let byteas: Vec<String> = [
        "''",
        "'\\x00'",
        "'\\x0000'",
        "'\\x01ff'",
        "'\\xff'",
        "'\\xdead'",
    ]
    .iter()
    .map(|s| format!("{s}::bytea"))
    .collect();

    for (label, ty, vals, queries) in [
        (
            "numeric",
            "NUMERIC",
            &nums,
            vec![
                "SELECT id FROM t WHERE v = 1.5 ORDER BY id",
                "SELECT id FROM t WHERE v = 2 ORDER BY id",
                "SELECT id FROM t WHERE v = 0 ORDER BY id",
                "SELECT id FROM t WHERE v > 1.5 ORDER BY id",
                "SELECT id FROM t WHERE v >= 1.5 ORDER BY id",
                "SELECT id FROM t WHERE v < 0 ORDER BY id",
                "SELECT id FROM t WHERE v BETWEEN -2 AND 2 ORDER BY id",
                "SELECT id FROM t WHERE v IN (1.5, -2, 0.001) ORDER BY id",
                "SELECT v FROM t ORDER BY v, id",
            ],
        ),
        (
            "bytea",
            "BYTEA",
            &byteas,
            vec![
                "SELECT id FROM t WHERE v = '\\x00'::bytea ORDER BY id",
                "SELECT id FROM t WHERE v > '\\x00'::bytea ORDER BY id",
                "SELECT id FROM t WHERE v < '\\xff'::bytea ORDER BY id",
                "SELECT id FROM t WHERE v BETWEEN '\\x00'::bytea AND '\\x01ff'::bytea ORDER BY id",
                "SELECT id FROM t WHERE v IN ('\\xff'::bytea, ''::bytea) ORDER BY id",
                "SELECT encode(v,'hex') FROM t ORDER BY v, id",
            ],
        ),
    ] {
        println!("== {label}");
        let mut scan = build(ty, vals, false);
        let mut idx = build(ty, vals, true);
        for q in queries {
            let a = rows(&mut scan, q);
            let b = rows(&mut idx, q);
            println!(
                "  {}  scan={:?} idx={:?}  {}",
                if a == b { "SAME " } else { "DIFF!" },
                a,
                b,
                q
            );
        }
        // Is the index actually engaged?
        for q in [
            "SELECT count(*) FROM t WHERE v = 1.5",
            "SELECT count(*) FROM t WHERE v = '\\x00'::bytea",
        ] {
            let mut e = build(ty, vals, true);
            let before = rows(
                &mut e,
                "SELECT idx_tup_fetch FROM pg_stat_user_tables WHERE relname='t'",
            );
            let _ = rows(&mut e, q);
            let after = rows(
                &mut e,
                "SELECT idx_tup_fetch FROM pg_stat_user_tables WHERE relname='t'",
            );
            if before != after {
                println!("  index engaged: {q}  ({:?} -> {:?})", before, after);
            }
        }
    }
}
