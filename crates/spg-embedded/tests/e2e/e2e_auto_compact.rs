//! v7.7.4 — auto-compact trigger in the background freezer.

use spg_embedded::{Database, FreezerOptions};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Scratch {
    path: PathBuf,
}
impl Scratch {
    fn new(label: &str) -> Self {
        let mut p = std::env::temp_dir();
        let nanos: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        p.push(format!(
            "spg-embedded-autocompact-{label}-{nanos}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self { path: p }
    }
    fn db_path(&self) -> PathBuf {
        self.path.join("app.db")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn freezer_produces_segments_then_compacts_them() {
    let scratch = Scratch::new("compact");
    let p = scratch.db_path();
    let db = Arc::new(Mutex::new(Database::open_path(&p).unwrap()));

    // Schema + bulk seed so the freezer has plenty to demote.
    {
        let mut g = db.lock().unwrap();
        g.execute("CREATE TABLE t (id INT NOT NULL, payload TEXT)")
            .unwrap();
        g.execute("CREATE INDEX t_pk ON t (id)").unwrap();
        for i in 0..2_000 {
            g.execute(&format!("INSERT INTO t VALUES ({i}, 'x')"))
                .unwrap();
        }
    }

    // Aggressive freezer: tiny hot budget, tiny batch, low
    // compact threshold so the test finishes quickly.
    let mut freezer = Database::spawn_background_freezer(
        db.clone(),
        FreezerOptions {
            tick: Duration::from_millis(10),
            hot_tier_bytes: 256,
            batch_rows: 20, // ~100 segments expected → compact must fire
            compact_when_segments_exceed: 8,
            compact_target_bytes: 1 << 30, // huge → all small segments mergeable
        },
    );

    // Contract: with threshold=8 and 2000 rows / 20-row batches,
    // a no-compact freezer would settle at ~100 segments. With
    // auto-compact, the population must stay near the threshold
    // (we allow a small overshoot — compact runs after freeze,
    // not interleaved with it).
    // 2 s window: 2000 rows / 20-row batches / 10 ms tick ≈ 1 s
    // to demote everything, +1 s of post-settle observation. (Was
    // a 5 s window — the single largest cost of this binary.)
    let mut max_seen = 0usize;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        let count = {
            let g = db.lock().unwrap();
            g.cold_segment_count()
        };
        max_seen = max_seen.max(count);
        std::thread::sleep(Duration::from_millis(20));
    }
    freezer.stop();
    // SPG's cold tier is a shadow model — full SELECT shows
    // hot only. To verify rows weren't lost we PK-probe a few
    // ids spread across the original insert range and confirm
    // they each surface (either still hot, or promoted back
    // from cold).
    for probe in [0, 500, 1000, 1500, 1999] {
        let count = {
            let mut g = db.lock().unwrap();
            match g
                .execute(&format!("SELECT id FROM t WHERE id = {probe}"))
                .unwrap()
            {
                spg_embedded::QueryResult::Rows { rows, .. } => rows.len(),
                _ => 0,
            }
        };
        assert_eq!(count, 1, "row id={probe} lost across compaction");
    }
    eprintln!("auto-compact test: max_seen={max_seen}");
    // The whole point: max segment count stays bounded near
    // the threshold even though 2000 rows / 20-row batches
    // would produce ~100 segments without compaction.
    assert!(
        max_seen <= 16,
        "auto-compact should bound segments near threshold; got max_seen={max_seen}"
    );
}

#[test]
fn auto_compact_disabled_when_threshold_is_max() {
    // With `compact_when_segments_exceed = usize::MAX`, the
    // freezer keeps producing segments without compacting.
    let scratch = Scratch::new("nocompact");
    let p = scratch.db_path();
    let db = Arc::new(Mutex::new(Database::open_path(&p).unwrap()));
    {
        let mut g = db.lock().unwrap();
        g.execute("CREATE TABLE t (id INT NOT NULL, payload TEXT)")
            .unwrap();
        g.execute("CREATE INDEX t_pk ON t (id)").unwrap();
        for i in 0..500 {
            g.execute(&format!("INSERT INTO t VALUES ({i}, 'x')"))
                .unwrap();
        }
    }
    let mut freezer = Database::spawn_background_freezer(
        db.clone(),
        FreezerOptions {
            tick: Duration::from_millis(20),
            hot_tier_bytes: 128,
            batch_rows: 50,
            compact_when_segments_exceed: usize::MAX,
            compact_target_bytes: 1 << 30,
        },
    );
    std::thread::sleep(Duration::from_millis(400));
    let count = {
        let g = db.lock().unwrap();
        g.cold_segment_count()
    };
    freezer.stop();
    assert!(
        count >= 4,
        "expected ≥ 4 segments when compaction disabled, got {count}"
    );
}
