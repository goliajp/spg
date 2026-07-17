//! Phase-A isolation probes for the DELETE 53×/13.5× write losses.
//! Both heavy_write DELETEs cost a flat ~27 ms regardless of matched
//! rows (500 vs 10k) — smells like a fixed per-statement rebuild.
//! Axes: matched-row count, index count, table size, tx wrapping.
//!
//! Run: `cargo run --release -p spg-bench-competitor --bin probe_delete`

#![allow(clippy::cast_precision_loss, clippy::uninlined_format_args)]

use std::time::Instant;

fn val_for(i: i64) -> i64 {
    ((i as u64).wrapping_mul(2_654_435_761) % 100_000) as i64
}

fn build(n: i64, indexes: usize) -> spg_engine::Engine {
    use spg_engine::Engine;
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE h (id INT NOT NULL, g INT NOT NULL, v INT NOT NULL)")
        .unwrap();
    if indexes >= 1 {
        eng.execute("CREATE INDEX h_v_idx ON h (v)").unwrap();
    }
    if indexes >= 2 {
        eng.execute("CREATE INDEX h_g_idx ON h (g)").unwrap();
    }
    for i in 1..=n {
        let (g, v) = (i % 100, val_for(i));
        eng.execute(&format!("INSERT INTO h VALUES ({i}, {g}, {v})"))
            .unwrap();
    }
    eng
}

fn timed(eng: &mut spg_engine::Engine, sql: &str, runs: usize) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..runs {
        eng.execute("BEGIN").unwrap();
        let t0 = Instant::now();
        eng.execute(sql).unwrap();
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        eng.execute("ROLLBACK").unwrap();
        if ms < best {
            best = ms;
        }
    }
    best
}

fn main() {
    println!("| probe | min ms |");
    println!("|-------|-------:|");
    // Axis 1: matched-row count on the 50k/2-index table.
    let mut e = build(50_000, 2);
    for (name, sql) in [
        ("del_1row_50k_2idx", "DELETE FROM h WHERE id = 25000"),
        ("del_0row_50k_2idx", "DELETE FROM h WHERE id = -1"),
        ("del_500_50k_2idx", "DELETE FROM h WHERE g = 50"),
        (
            "del_10k_50k_2idx",
            "DELETE FROM h WHERE v BETWEEN 20000 AND 40000",
        ),
    ] {
        println!("| {:<20} | {:>7.3} |", name, timed(&mut e, sql, 7));
    }
    // Axis 2: index count.
    let mut e1 = build(50_000, 1);
    println!(
        "| {:<20} | {:>7.3} |",
        "del_500_50k_1idx",
        timed(&mut e1, "DELETE FROM h WHERE g = 50", 7)
    );
    let mut e0 = build(50_000, 0);
    println!(
        "| {:<20} | {:>7.3} |",
        "del_500_50k_0idx",
        timed(&mut e0, "DELETE FROM h WHERE g = 50", 7)
    );
    // Axis 3: table size (2 indexes).
    let mut e5k = build(5_000, 2);
    println!(
        "| {:<20} | {:>7.3} |",
        "del_50_5k_2idx",
        timed(&mut e5k, "DELETE FROM h WHERE g = 50", 7)
    );
    // Axis 5: MVCC in-place gate ON — the tombstone write path (the
    // v7.37.15 epic's endgame). Confirms "finish the epic ⇒ the DELETE
    // loss closes" before treating the epic as the fix.
    let mut em = build(50_000, 2);
    em.set_mvcc_inplace(true);
    for (name, sql) in [
        ("del_500_50k_2idx_MVCC", "DELETE FROM h WHERE g = 50"),
        (
            "del_10k_50k_2idx_MVCC",
            "DELETE FROM h WHERE v BETWEEN 20000 AND 40000",
        ),
    ] {
        println!("| {:<20} | {:>7.3} |", name, timed(&mut em, sql, 7));
    }
    // Axis 6: MVCC gate-on WITH redo capture — the persistent-database
    // shape (the redo-off axis above can't see the per-row vs batch
    // Tombstone record cost at all).
    let mut er = build(50_000, 2);
    er.set_mvcc_inplace(true);
    er.set_redo_capture(true);
    for (name, sql) in [
        ("del_500_MVCC_redo", "DELETE FROM h WHERE g = 50"),
        (
            "del_10k_MVCC_redo",
            "DELETE FROM h WHERE v BETWEEN 20000 AND 40000",
        ),
    ] {
        let mut best = f64::MAX;
        for _ in 0..7 {
            er.execute("BEGIN").unwrap();
            let t0 = Instant::now();
            er.execute(sql).unwrap();
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            er.execute("ROLLBACK").unwrap();
            let _ = er.take_redo();
            if ms < best {
                best = ms;
            }
        }
        println!("| {:<20} | {:>7.3} |", name, best);
    }
    // Axis 4: autocommit (no explicit tx) — is BEGIN..ROLLBACK itself the tax?
    let mut ea = build(50_000, 2);
    let mut best = f64::MAX;
    for _ in 0..7 {
        let t0 = Instant::now();
        ea.execute("DELETE FROM h WHERE g = 50").unwrap();
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        if ms < best {
            best = ms;
        }
        // restore the 500 rows so each run deletes the same set
        for i in (1..=50_000i64).filter(|i| i % 100 == 50) {
            ea.execute(&format!("INSERT INTO h VALUES ({i}, 50, {})", val_for(i)))
                .unwrap();
        }
    }
    println!("| {:<20} | {:>7.3} |", "del_500_autocommit", best);
}
