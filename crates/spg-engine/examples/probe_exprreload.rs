//! Does an expression index survive a restart?
//!
//! The completeness flag is not persisted, on purpose: an older catalog
//! holds the leading column's values under an expression index and must
//! not answer with them. So the restore path has to refill it, or the
//! index is inert from restart until the table's next write.
use spg_engine::{Engine, QueryResult};

fn idx_scan(e: &mut Engine, t: &str) -> i64 {
    match e
        .execute(&format!(
            "SELECT idx_scan FROM pg_stat_user_tables WHERE relname = '{t}'"
        ))
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => match rows.first().map(|r| r.values[0].clone()) {
            Some(spg_storage::Value::BigInt(n)) => n,
            other => panic!("{other:?}"),
        },
        _ => unreachable!(),
    }
}

fn main() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE r (k INT, s TEXT)").unwrap();
    for i in 0..500 {
        e.execute(&format!("INSERT INTO r VALUES ({i}, 'X{i}')"))
            .unwrap();
    }
    e.execute("CREATE INDEX r_lower ON r (lower(s))").unwrap();
    let bytes = e.snapshot();

    let mut e2 = Engine::restore_envelope(&bytes).unwrap();
    let before = idx_scan(&mut e2, "r");
    let n = match e2
        .execute("SELECT k FROM r WHERE lower(s) = 'x42'")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => rows.len(),
        _ => unreachable!(),
    };
    println!(
        "after restore: {n} row, idx_scan {before} -> {}",
        idx_scan(&mut e2, "r")
    );
}
