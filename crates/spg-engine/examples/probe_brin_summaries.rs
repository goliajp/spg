//! v7.38.11 — what does the BRIN prune actually buy, in process?
//!
//! The control is the same table and the same query WITHOUT the index:
//! anything the prune is worth has to show up as the difference.
use spg_engine::Engine;
use std::time::Instant;

fn build(with_brin: bool, n: i64) -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, ts TIMESTAMP NOT NULL)")
        .unwrap();
    if with_brin {
        e.execute("CREATE INDEX t_brin ON t USING brin (ts)")
            .unwrap();
    }
    e.execute(&format!(
        "INSERT INTO t SELECT g, timestamp '2026-01-01 00:00:00' + ((g) || ' minutes')::interval \
         FROM generate_series(1, {n}) g"
    ))
    .unwrap();
    e
}

fn time(e: &mut Engine, sql: &str) -> (f64, String) {
    e.execute(sql).unwrap();
    let t = Instant::now();
    let mut out = String::new();
    for _ in 0..5 {
        let r = e.execute(sql).unwrap();
        out = format!("{:?}", r)
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect();
    }
    (t.elapsed().as_secs_f64() * 1000.0 / 5.0, out)
}

/// The NEGATIVE control: timestamps that cycle instead of ascending.
/// Correlation collapses, every range spans the whole span, and a
/// correct prune must skip NOTHING — so this pair must stay level. A
/// version that speeds this up is skipping rows it should not, and it
/// would look like a bigger win than the correct one.
fn build_cycling(with_brin: bool, n: i64) -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, ts TIMESTAMP NOT NULL)")
        .unwrap();
    if with_brin {
        e.execute("CREATE INDEX t_brin ON t USING brin (ts)")
            .unwrap();
    }
    e.execute(&format!(
        "INSERT INTO t SELECT g, timestamp '2026-01-01 00:00:00' + ((g % 1440) || ' minutes')::interval \
         FROM generate_series(1, {n}) g"
    ))
    .unwrap();
    e
}

fn main() {
    let n = 200_000;
    let q =
        "SELECT count(*) FROM t WHERE ts >= timestamp '2026-01-02' AND ts < timestamp '2026-01-03'";
    let (t_no, a_no) = time(&mut build(false, n), q);
    let (t_yes, a_yes) = time(&mut build(true, n), q);
    println!("ascending  — no brin {t_no:.3} ms | brin {t_yes:.3} ms | answer {a_no}");
    assert_eq!(a_no, a_yes, "the prune changed the answer");

    let qc = "SELECT count(*) FROM t WHERE ts >= timestamp '2026-01-01 06:00:00' AND ts < timestamp '2026-01-01 07:00:00'";
    let (c_no, b_no) = time(&mut build_cycling(false, n), qc);
    let (c_yes, b_yes) = time(&mut build_cycling(true, n), qc);
    println!("cycling    — no brin {c_no:.3} ms | brin {c_yes:.3} ms | answer {b_no}");
    assert_eq!(b_no, b_yes, "the prune changed the answer on the control");
    assert!(
        c_yes > c_no * 0.5,
        "the control got FASTER with an index that cannot prune it — \
         {c_no:.3} ms to {c_yes:.3}. That is rows being skipped that should not be."
    );
}
