//! v7.38.13 Phase A — the customer's `dashboard: top versions` shape,
//! in process, so a profiler sees engine work and not a container.
//!
//! Two knobs, because the first Phase A priced the accessor with a
//! vehicle that turned out not to take the path it then changed:
//!
//!   * `SPG_PROBE_SHAPE=accessor` (default) groups on `traits->>'version'`
//!   * `SPG_PROBE_SHAPE=plain`    groups on a plain TEXT column of the
//!     SAME cardinality — the only difference is where the key comes
//!     from, so the subtraction prices the accessor and nothing else.
//!
//! The equal-cardinality control matters: grouping on a literal
//! accessor collapses 40 groups into 1 and prices the grouping instead.
use spg_engine::Engine;
use std::time::Instant;

fn build(n: i64) -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE events (id BIGINT NOT NULL, project_id INT NOT NULL, ver TEXT NOT NULL, traits JSONB NOT NULL)")
        .unwrap();
    // `ver` carries the same 40 distinct values the accessor yields, so
    // the two shapes below build the same group table.
    e.execute(&format!(
        "INSERT INTO events SELECT g, (g % 8) + 1, ((g % 40) + 1)::text, \
         jsonb_build_object('plan', 'pro', 'seat', g % 500, 'country', 'jp', \
                            'version', ((g % 40) + 1)::text) \
         FROM generate_series(1, {n}) g"
    ))
    .unwrap();
    e
}

fn time(e: &mut Engine, sql: &str, reps: usize) -> f64 {
    e.execute(sql).unwrap();
    let t = Instant::now();
    for _ in 0..reps {
        e.execute(sql).unwrap();
    }
    t.elapsed().as_secs_f64() * 1000.0 / reps as f64
}

fn main() {
    let n: i64 = std::env::var("SPG_PROBE_ROWS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200_000);
    let reps: usize = std::env::var("SPG_PROBE_REPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let shape = std::env::var("SPG_PROBE_SHAPE").unwrap_or_else(|_| "both".into());

    let accessor = "SELECT traits->>'version' AS v, count(*) FROM events \
                    WHERE project_id = 3 GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT 10";
    let plain = "SELECT ver AS v, count(*) FROM events \
                 WHERE project_id = 3 GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT 10";

    let mut e = build(n);
    let rows: i64 = n / 8;
    match shape.as_str() {
        // Single-shape modes exist so a profiler records ONE of them.
        "accessor" => {
            let ms = time(&mut e, accessor, reps);
            println!(
                "accessor {ms:.3} ms  ({:.0} ns/row)",
                ms * 1e6 / rows as f64
            );
        }
        "plain" => {
            let ms = time(&mut e, plain, reps);
            println!(
                "plain    {ms:.3} ms  ({:.0} ns/row)",
                ms * 1e6 / rows as f64
            );
        }
        _ => {
            // Interleaved with a ROTATING start, so a drifting machine
            // cannot bias one leg, and reported as a BAND: a difference
            // inside the band is not a result. The first cut of this
            // probe reported a bare median and read a 13 % spread as a
            // 7 % win.
            let rounds: usize = std::env::var("SPG_PROBE_ROUNDS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(9);
            let (mut a, mut p) = (Vec::new(), Vec::new());
            for i in 0..rounds {
                if i % 2 == 0 {
                    a.push(time(&mut e, accessor, reps));
                    p.push(time(&mut e, plain, reps));
                } else {
                    p.push(time(&mut e, plain, reps));
                    a.push(time(&mut e, accessor, reps));
                }
            }
            a.sort_by(f64::total_cmp);
            p.sort_by(f64::total_cmp);
            let band = |v: &[f64]| (v[0], v[v.len() / 2], v[v.len() - 1]);
            let (alo, am, ahi) = band(&a);
            let (plo, pm, phi) = band(&p);
            println!(
                "accessor {am:.3} ms   [{alo:.3} .. {ahi:.3}]  spread {:.1}%",
                100.0 * (ahi - alo) / am
            );
            println!(
                "plain    {pm:.3} ms   [{plo:.3} .. {phi:.3}]  spread {:.1}%",
                100.0 * (phi - plo) / pm
            );
            println!(
                "accessor costs {:.3} ms over {rows} rows = {:.0} ns/row",
                am - pm,
                (am - pm) * 1e6 / rows as f64
            );
        }
    }
}
