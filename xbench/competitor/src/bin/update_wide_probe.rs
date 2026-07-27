//! v7.39 (round 556/557) — where update_wide's 2x lives.
//!
//! Round 556 established, counter-first, that it is NOT index
//! maintenance: dropping the index on the updated column recovers 9%
//! and updating a non-indexed column instead recovers 2%.
//!
//! Round 557 asks the next question — does the per-row cost STAY FLAT
//! as the updated row count grows? A flat µs/row means a linear path
//! that is merely slow; a rising one means the work per row depends on
//! how many rows the statement touches, which is a different defect
//! and a different fix.
use spg_engine::Engine;
use std::time::Instant;

/// `pad_len` widens the row without touching anything else, so a cost
/// that tracks it is a cost that copies the whole row.
fn seed_wide(with_v_index: bool, pad_len: usize) -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE h (id INT NOT NULL, g INT NOT NULL, v INT NOT NULL, pad TEXT)")
        .unwrap();
    e.execute(&format!(
        "INSERT INTO h SELECT i, i % 100, i, repeat('x', {pad_len}) FROM generate_series(1, 50000) i"
    ))
    .unwrap();
    if with_v_index {
        e.execute("CREATE INDEX h_v_idx ON h (v)").unwrap();
    }
    e.execute("CREATE INDEX h_g_idx ON h (g)").unwrap();
    e
}

fn best_ms(with_v_index: bool, sql: &str) -> f64 {
    best_ms_wide(with_v_index, 1, sql)
}

fn best_ms_wide(with_v_index: bool, pad_len: usize, sql: &str) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..7 {
        let mut e = seed_wide(with_v_index, pad_len);
        let t = Instant::now();
        e.execute(sql).unwrap();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        if ms < best {
            best = ms;
        }
    }
    best
}

fn main() {
    println!("== round 556: is it index maintenance?  (no: 9% at most)");
    for (label, idx, sql) in [
        ("A v-index, SET v (the filter col)", true, "UPDATE h SET v = v + 1 WHERE v < 50000"),
        ("B v-index, SET pad (not indexed) ", true, "UPDATE h SET pad = 'y' WHERE v < 50000"),
        ("C no v-index, SET v              ", false, "UPDATE h SET v = v + 1 WHERE v < 50000"),
        ("D v-index, SET v, 500 rows       ", true, "UPDATE h SET v = v + 1 WHERE g = 50"),
    ] {
        println!("{label}  {:>9.2} ms", best_ms(idx, sql));
    }

    println!();
    println!("== round 557: does the per-row cost stay flat as N grows?");
    println!("| rows  | UPDATE ms | us/row | matching SELECT ms | us/row |");
    for n in [500i64, 2_000, 5_000, 10_000, 25_000] {
        let upd = best_ms(true, &format!("UPDATE h SET v = v + 1 WHERE v <= {n}"));
        // The same predicate, read-only: separates finding the rows
        // from writing them.
        let sel = best_ms(true, &format!("SELECT count(*) FROM h WHERE v <= {n}"));
        println!(
            "| {n:>5} | {upd:>9.2} | {:>6.2} | {sel:>18.3} | {:>6.2} |",
            upd * 1000.0 / n as f64,
            sel * 1000.0 / n as f64
        );
    }

    println!();
    println!("== round 557: does the cost track the ROW WIDTH?");
    println!("(a cost that does is a cost that copies the whole row per update)");
    println!("| pad bytes | UPDATE 10k rows ms | us/row |");
    for pad in [1usize, 100, 400, 1000] {
        let ms = best_ms_wide(true, pad, "UPDATE h SET v = v + 1 WHERE v <= 10000");
        println!("| {pad:>9} | {ms:>18.2} | {:>6.2} |", ms * 1000.0 / 10_000.0);
    }

    println!();
    println!("== round 557: SET v = <constant> makes 10k IDENTICAL index keys");
    for (label, idx, sql) in [
        ("v INDEXED,   SET v = 1        (10k dupes)", true, "UPDATE h SET v = 1 WHERE v <= 10000"),
        ("v UNindexed, SET v = 1        (10k dupes)", false, "UPDATE h SET v = 1 WHERE v <= 10000"),
        ("v INDEXED,   SET v = v + 1    (distinct) ", true, "UPDATE h SET v = v + 1 WHERE v <= 10000"),
        ("v UNindexed, SET v = v + 1    (distinct) ", false, "UPDATE h SET v = v + 1 WHERE v <= 10000"),
    ] {
        println!("{label}  {:>9.2} ms", best_ms(idx, sql));
    }

    println!();
    println!("== round 557: is it the SET expression?");
    for (label, sql) in [
        ("SET v = v + 1 (reads the old value)", "UPDATE h SET v = v + 1 WHERE v <= 10000"),
        ("SET v = 1     (a constant)         ", "UPDATE h SET v = 1 WHERE v <= 10000"),
        ("SET pad = 'y' (a constant, no idx) ", "UPDATE h SET pad = 'y' WHERE v <= 10000"),
    ] {
        println!("{label}  {:>9.2} ms", best_ms(true, sql));
    }
}
