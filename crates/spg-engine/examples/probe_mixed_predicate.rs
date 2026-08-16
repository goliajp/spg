//! r1038 — does letting one-sided ranges reach the index cost the
//! equality seek beside them?
//!
//! r1035 made `col <= x` seekable, which mailrs needed. The range attempt
//! runs BEFORE the AND recursion that finds equalities, so a permissive
//! parse — one that takes a range out of a predicate that also contains
//! other conjuncts — would make
//!
//! ```sql
//! WHERE id = 7 AND ts > <threshold>
//! ```
//!
//! walk the `ts` range instead of seeking the one row `id` names. The
//! selectivity cap does not catch it: a range matching a tenth of the
//! table passes the cap comfortably and is still ten thousand times wider
//! than the equality.
//!
//! Measured by SHAPE, not by a stopwatch reading, for the reason
//! `probe_range_index` gives: hold the answer at one row and grow the
//! table. An equality seek is flat; a range walk is linear.
//!
//!   cargo run --release --example probe_mixed_predicate

use spg_engine::Engine;
use std::time::Instant;

/// A tenth of the table is above the threshold — well inside the
/// quarter-of-the-table cap, so the cap alone will not refuse it.
fn build(rows: i64) -> Engine {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE ev (id BIGINT PRIMARY KEY, ts BIGINT, pad TEXT)")
        .unwrap();
    eng.execute("CREATE INDEX ev_ts ON ev (ts)").unwrap();
    let mut values = String::new();
    for i in 0..rows {
        if !values.is_empty() {
            values.push(',');
        }
        // ts ascends with id, so `ts > 90% of max` is the last tenth.
        values.push_str(&format!("({i},{i},'x')"));
        if values.len() > 60_000 || i == rows - 1 {
            eng.execute(&format!("INSERT INTO ev (id, ts, pad) VALUES {values}"))
                .unwrap();
            values.clear();
        }
    }
    eng
}

fn time(eng: &mut Engine, sql: &str) -> f64 {
    for _ in 0..3 {
        eng.execute(sql).unwrap();
    }
    let t = Instant::now();
    for _ in 0..20 {
        eng.execute(sql).unwrap();
    }
    t.elapsed().as_secs_f64() * 1000.0 / 20.0
}

fn main() {
    println!("   the answer is ONE row in every case; only the table grows.");
    println!("   flat => the equality seek. linear => the range walk took it.");
    println!("   rows      id = 7 alone   id = 7 AND ts > 90%");
    let mut first = (0.0f64, 0.0f64);
    for (n, rows) in [20_000i64, 40_000, 80_000, 160_000].iter().enumerate() {
        let mut eng = build(*rows);
        let cut = rows * 9 / 10;
        let eq = time(&mut eng, "SELECT id FROM ev WHERE id = 7");
        let mixed = time(
            &mut eng,
            &format!("SELECT id FROM ev WHERE id = 7 AND ts > {cut}"),
        );
        if n == 0 {
            first = (eq, mixed);
        }
        println!(
            "   {rows:<9} {eq:>8.3} ms      {mixed:>8.3} ms   (x{:.2} of the first)",
            mixed / first.1
        );
    }
    // A control: the same range with no equality beside it SHOULD walk the
    // index, which is the r1035 change this must not undo.
    let mut eng = build(160_000);
    let cut = 160_000 * 9 / 10;
    println!(
        "\n   control — range alone at 160k: {:.3} ms (a tenth of the table)",
        time(&mut eng, &format!("SELECT id FROM ev WHERE ts > {cut}"))
    );
}
