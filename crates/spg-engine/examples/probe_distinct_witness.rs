//! v7.38.14 Phase A — before pricing two spellings against each other,
//! prove they are doing the same work. The sizing probe only `unwrap()`s;
//! a spelling that returns fewer rows would look exactly like a fast one.
use spg_engine::{Engine, QueryResult};

fn build(n: i64) -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE big (id INT PRIMARY KEY, k INT, pad TEXT)")
        .unwrap();
    e.execute(&format!(
        "INSERT INTO big SELECT g, ((g::bigint*7919)%{n})::int, \
         repeat(chr(97+(g%26)),200) FROM generate_series(1,{n}) g"
    ))
    .unwrap();
    e
}

fn digest(r: &QueryResult) -> (usize, u64, String, String) {
    match r {
        QueryResult::Rows { rows, .. } => {
            // Order-sensitive fold: a different ORDER would change it.
            let mut h: u64 = 1469598103934665603;
            for row in rows {
                for v in &row.values {
                    for b in format!("{v:?}").bytes() {
                        h ^= u64::from(b);
                        h = h.wrapping_mul(1099511628211);
                    }
                }
            }
            let f = |i: usize| format!("{:?}", rows[i].values);
            (rows.len(), h, f(0), f(rows.len() - 1))
        }
        other => (0, 0, format!("{other:?}"), String::new()),
    }
}

fn main() {
    let n: i64 = std::env::var("SPG_PROBE_ROWS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(400_000);
    let mut e = build(n);
    for (name, sql) in [
        ("distinct", "SELECT DISTINCT k FROM big ORDER BY k"),
        ("groupby ", "SELECT k FROM big GROUP BY k ORDER BY k"),
        ("order   ", "SELECT k FROM big ORDER BY k"),
    ] {
        let (rows, h, first, last) = digest(&e.execute(sql).unwrap());
        println!("{name}  rows={rows:<7} fold={h:016x}  first={first}  last={last}");
    }
}
