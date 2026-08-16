//! r1040 — does `ORDER BY <numeric>` hold past f64's 15 significant digits?
//!
//! `OrderKey::Num(f64)` is the sort key for `Value::Numeric`. Round 664
//! added `OrderKey::Int(i128)` because an f64 projection collapses
//! adjacent BigInt/Timestamp values past 2^53; NUMERIC never got the same
//! treatment, and `NumericBig` (the i128-overflow form) has its own exact
//! key. So the gap, if there is one, is the ORDINARY numeric.
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

fn main() {
    let vals = [
        "9007199254740993", // 2^53 + 1
        "9007199254740992", // 2^53
        "9007199254740994",
        "0.1000000000000000001",
        "0.1000000000000000002",
        "0.1",
        "123456789012345678.9",
        "123456789012345678.8",
        "-9007199254740993",
        "-9007199254740992",
    ];
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT PRIMARY KEY, v NUMERIC)")
        .unwrap();
    for (i, v) in vals.iter().enumerate() {
        e.execute(&format!("INSERT INTO t VALUES ({}, {v})", i + 1))
            .unwrap();
    }
    println!("ORDER BY v:");
    for r in rows(&mut e, "SELECT v FROM t ORDER BY v") {
        println!("   {r}");
    }
    println!("\nDISTINCT count (all 10 values are distinct):");
    println!(
        "   {:?}",
        rows(&mut e, "SELECT count(*) FROM (SELECT DISTINCT v FROM t) s")
    );
    println!("\nmin / max:");
    println!("   {:?}", rows(&mut e, "SELECT min(v), max(v) FROM t"));
    println!("\ncomparison (should be true):");
    for q in [
        "SELECT 9007199254740993::numeric > 9007199254740992::numeric",
        "SELECT 0.1000000000000000002::numeric > 0.1000000000000000001::numeric",
    ] {
        println!("   {q} -> {:?}", rows(&mut e, q));
    }
}
