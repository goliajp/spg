//! v7.39 (round 556) — where update_wide's 2x lives. Counter-first,
//! no code change: separate index maintenance on the UPDATED column
//! from the update path itself.
use spg_engine::Engine;
use std::time::Instant;

fn seed(with_v_index: bool) -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE h (id INT NOT NULL, g INT NOT NULL, v INT NOT NULL, pad TEXT)")
        .unwrap();
    e.execute(
        "INSERT INTO h SELECT i, i % 100, i, 'x' FROM generate_series(1, 50000) i",
    )
    .unwrap();
    if with_v_index {
        e.execute("CREATE INDEX h_v_idx ON h (v)").unwrap();
    }
    e.execute("CREATE INDEX h_g_idx ON h (g)").unwrap();
    e
}

fn timed(label: &str, with_v_index: bool, sql: &str) {
    let mut best = f64::MAX;
    for _ in 0..7 {
        let mut e = seed(with_v_index);
        let t = Instant::now();
        e.execute(sql).unwrap();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        if ms < best {
            best = ms;
        }
    }
    println!("{label:<46} {best:>9.2} ms");
}

fn main() {
    // A: the panel's shape — the filtered column IS the updated one.
    timed("A v-index, SET v (filter col) WHERE v<50000", true, "UPDATE h SET v = v + 1 WHERE v < 50000");
    // B: same rows, same predicate, a NON-indexed column updated.
    timed("B v-index, SET pad          WHERE v<50000", true, "UPDATE h SET pad = 'y' WHERE v < 50000");
    // C: no index on v at all — isolates index maintenance entirely.
    timed("C no v-index, SET v         WHERE v<50000", false, "UPDATE h SET v = v + 1 WHERE v < 50000");
    // D: the narrow shape, which already wins.
    timed("D v-index, SET v            WHERE g=50", true, "UPDATE h SET v = v + 1 WHERE g = 50");
}
