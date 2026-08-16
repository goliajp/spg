//! r1035 — mailrs reports a range predicate not reaching an index that
//! equality does reach (`spg-reactivation-measured-2026-08-16`).
//!
//! Their evidence is `EXPLAIN` output:
//!
//! ```text
//! scheduled_at = 1000000          -> Index Scan using idx_...
//! scheduled_at <= 1755000000      -> Seq Scan + Sort + Limit, rows=6666
//! ```
//!
//! with `rows = 6666` — exactly `20000 / 3` — coming back byte-identical
//! on fixtures whose selectivity differs 200-fold, and `ANALYZE` changing
//! nothing.
//!
//! Two separate questions, and the plan text answers only the first:
//!
//! 1. **Does the PLAN say index?** That is what they measured.
//! 2. **Does the EXECUTOR use one?** A plan description that does not
//!    reflect the executor's fast paths would make (1) a display defect
//!    and (2) fine. This round already found the opposite mistake — a lane
//!    that engaged in one configuration and not another, invisible from
//!    the outside — so neither is assumed here.
//!
//! (2) is answered by SHAPE, not by a stopwatch reading: hold the number
//! of MATCHING rows fixed and grow the table. An index walk is flat; a
//! sequential scan is linear. That distinction survives a noisy machine,
//! which an absolute millisecond figure would not.
//!
//!   cargo run --release --example probe_range_index

use spg_engine::Engine;

/// Matching rows, held constant across every table size — the workload's
/// shape, per mailrs: "almost nothing is scheduled".
const MATCHES: i64 = 50;

fn build(rows: i64) -> Engine {
    build_with(rows, MATCHES)
}

fn build_with(rows: i64, scheduled: i64) -> Engine {
    let mut eng = Engine::new();
    eng.execute(
        "CREATE TABLE outbound_queue (
            id BIGINT PRIMARY KEY,
            scheduled_at BIGINT,
            status TEXT NOT NULL,
            payload TEXT NOT NULL
         )",
    )
    .expect("create");
    eng.execute("CREATE INDEX idx_outbound_scheduled_at ON outbound_queue (scheduled_at)")
        .expect("index");
    let mut sql = String::with_capacity(1 << 20);
    let mut i = 1;
    while i <= rows {
        sql.clear();
        sql.push_str("INSERT INTO outbound_queue VALUES ");
        let end = (i + 4_999).min(rows);
        for g in i..=end {
            if g > i {
                sql.push(',');
            }
            // The first MATCHES rows are scheduled; everything else is
            // NULL, which is what "almost nothing is scheduled" means.
            if g <= scheduled {
                sql.push_str(&format!("({g},{},'pending','p{g}')", g * 1000));
            } else {
                sql.push_str(&format!("({g},NULL,'pending','p{g}')"));
            }
        }
        eng.execute(&sql).expect("seed");
        i = end + 1;
    }
    eng
}

fn explain(eng: &mut Engine, sql: &str) -> String {
    let out = eng
        .execute(&format!("EXPLAIN {sql}"))
        .unwrap_or_else(|e| panic!("EXPLAIN failed: {e:?}"));
    format!("{out:?}")
}

/// First line of a plan, which is the node that decides scan vs index.
fn head(plan: &str) -> String {
    plan.split("\\n")
        .next()
        .unwrap_or(plan)
        .chars()
        .take(120)
        .collect()
}

fn time_ms(eng: &mut Engine, sql: &str, reps: u32) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..reps {
        let t0 = std::time::Instant::now();
        let out = eng.execute(sql).expect("query");
        best = best.min(t0.elapsed().as_secs_f64() * 1000.0);
        core::hint::black_box(&out);
    }
    best
}

const EQ: &str = "SELECT id FROM outbound_queue WHERE scheduled_at = 1000";
const RANGE: &str = "SELECT id FROM outbound_queue \
                     WHERE scheduled_at IS NOT NULL AND scheduled_at <= 1755000000 \
                     ORDER BY scheduled_at LIMIT 100";

/// Which SHAPE loses the index? The reported query carries three things
/// at once — a range, an `IS NOT NULL` beside it, and an ORDER BY with a
/// LIMIT on the same column — and any one of them could be the gate.
/// Timed at two table sizes with the matching rows held constant, so the
/// ratio names the scanners without trusting a clock.
fn shape_sweep() {
    let shapes: [(&str, &str); 8] = [
        (
            "equality",
            "SELECT id FROM outbound_queue WHERE scheduled_at = 1000",
        ),
        (
            "range alone",
            "SELECT id FROM outbound_queue WHERE scheduled_at <= 1755000000",
        ),
        (
            "range + IS NOT NULL",
            "SELECT id FROM outbound_queue WHERE scheduled_at IS NOT NULL AND scheduled_at <= 1755000000",
        ),
        (
            "range + ORDER BY",
            "SELECT id FROM outbound_queue WHERE scheduled_at <= 1755000000 ORDER BY scheduled_at",
        ),
        (
            "range + ORDER BY + LIMIT",
            "SELECT id FROM outbound_queue WHERE scheduled_at <= 1755000000 ORDER BY scheduled_at LIMIT 100",
        ),
        ("the reported query", RANGE),
        // The negative control for r1035: a range matching EVERY row must
        // stay on the scan. The selectivity cap is what is supposed to
        // refuse it, and letting one-sided ranges through is only safe if
        // that holds.
        (
            "wide range (all rows match)",
            "SELECT id FROM outbound_queue WHERE id <= 999999999",
        ),
        (
            "wide range, other direction",
            "SELECT id FROM outbound_queue WHERE id >= 0",
        ),
    ];
    println!("\n== which shape loses the index?");
    println!(
        "   {:<28} {:>9} {:>9} {:>8}",
        "shape", "20k ms", "160k ms", "x"
    );
    for (label, sql) in shapes {
        let mut small = build(20_000);
        let mut big = build(160_000);
        let a = time_ms(&mut small, sql, 5);
        let b = time_ms(&mut big, sql, 5);
        let verdict = if b / a > 4.0 { "SCAN" } else { "index" };
        println!("   {label:<28} {a:>9.3} {b:>9.3} {:>7.2}  {verdict}", b / a);
    }
}

fn main() {
    println!("== what the PLAN says (20,000 rows, 50 scheduled)");
    let mut eng = build(20_000);
    println!("  equality : {}", head(&explain(&mut eng, EQ)));
    println!("  range    : {}", head(&explain(&mut eng, RANGE)));

    // mailrs: "the estimate and every cost came back byte-identical" across a
    // 200-fold change in selectivity. Vary the MATCHING rows, not the table
    // size — that is the axis the estimate is supposed to follow.
    println!("\n== does the estimate move with selectivity? (20,000 rows in every case)");
    for scheduled in [50i64, 5_000, 10_000] {
        let mut e = build_with(20_000, scheduled);
        let plan = explain(
            &mut e,
            "SELECT id FROM outbound_queue WHERE scheduled_at <= 1755000000",
        );
        let est = plan
            .split("rows=")
            .nth(1)
            .map(|r| {
                r.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
            })
            .unwrap_or_else(|| "?".into());
        println!("  {scheduled:>6} of 20,000 match -> rows={est}");
    }

    println!("\n== what the EXECUTOR does");
    println!("   matching rows are held at {MATCHES}; only the table grows.");
    println!("   flat => an index walk. linear => a scan.");
    println!("   {:<10} {:>10} {:>10}", "rows", "range ms", "equality ms");
    let mut first: Option<f64> = None;
    for rows in [20_000i64, 40_000, 80_000, 160_000] {
        let mut e = build(rows);
        let r = time_ms(&mut e, RANGE, 5);
        let q = time_ms(&mut e, EQ, 5);
        let ratio = first.map_or(1.0, |f: f64| r / f);
        if first.is_none() {
            first = Some(r);
        }
        println!("   {rows:<10} {r:>10.3} {q:>10.3}   (range x{ratio:.2} of the first)");
    }
    shape_sweep();
}
