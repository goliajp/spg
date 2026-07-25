//! P0-13 — is a range DELETE doing a full scan? (yes)
//!
//! Run: cargo run --release -p spg-bench-competitor --bin del_range_scaling
//!
//! `DELETE FROM wb WHERE id >= a AND id < a+1000` on the PK costs SPGS
//! 4.967 ms against PG18's 0.292 ms — 17x, and the whole of
//! `delete_reinsert_1k`'s loss. If the predicate is not reaching the index,
//! the cost scales with the TABLE, not with the 1000 rows removed. Embedded,
//! so the wire is out of it.
use spg_engine::Engine;
use std::fmt::Write as _;
use std::time::Instant;

fn batch_sql(base: i64, rows: i64) -> String {
    let mut s = String::with_capacity(rows as usize * 24 + 32);
    s.push_str("INSERT INTO wb VALUES ");
    for k in 0..rows {
        let id = base + k;
        if k > 0 {
            s.push(',');
        }
        let _ = write!(s, "({id},{},{})", id % 100, id * 7 % 100_000);
    }
    s
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn run(total: i64) -> (f64, f64, f64, f64, f64, f64) {
    let mut e = Engine::new();
    e.execute("CREATE TABLE wb(id INT PRIMARY KEY, g INT, v INT)")
        .unwrap();
    for chunk in 0..(total / 1000) {
        e.execute(&batch_sql(chunk * 1000, 1000)).unwrap();
    }
    let seg = total / 2;
    let del = format!("DELETE FROM wb WHERE id >= {seg} AND id < {}", seg + 1000);
    let ins = batch_sql(seg, 1000);
    let eq = format!("DELETE FROM wb WHERE id = {}", seg + 500);
    let eq_ins = format!("INSERT INTO wb VALUES ({},1,1)", seg + 500);
    // A RANGE predicate matching exactly one row: separates "does the range
    // seek fire" from "does applying N deletes cost O(table)".
    let r1 = format!(
        "DELETE FROM wb WHERE id >= {} AND id < {}",
        seg + 700,
        seg + 701
    );
    let r1_ins = format!("INSERT INTO wb VALUES ({},1,1)", seg + 700);
    for _ in 0..3 {
        e.execute(&del).unwrap();
        e.execute(&ins).unwrap();
    }
    let sel = format!(
        "SELECT count(*) FROM wb WHERE id >= {} AND id < {}",
        seg + 700,
        seg + 701
    );
    let upd = format!(
        "UPDATE wb SET v = v WHERE id >= {} AND id < {}",
        seg + 700,
        seg + 701
    );
    // Equality UPDATE: same one row, same mutation, no range predicate.
    // If this is flat the cost tracks the WHERE shape; if it scales, the
    // flat one is equality-DELETE's own fast path.
    let upd_eq = format!("UPDATE wb SET v = v WHERE id = {}", seg + 700);
    let mut rv = Vec::new();
    let mut ev = Vec::new();
    let mut r1v = Vec::new();
    let mut sv = Vec::new();
    let mut uv = Vec::new();
    let mut uev = Vec::new();
    for _ in 0..21 {
        let t = Instant::now();
        e.execute(&del).unwrap();
        rv.push(t.elapsed().as_secs_f64() * 1000.0);
        e.execute(&ins).unwrap();
        let t = Instant::now();
        e.execute(&eq).unwrap();
        ev.push(t.elapsed().as_secs_f64() * 1000.0);
        e.execute(&eq_ins).unwrap();
        let t = Instant::now();
        e.execute(&r1).unwrap();
        r1v.push(t.elapsed().as_secs_f64() * 1000.0);
        e.execute(&r1_ins).unwrap();
        let t = Instant::now();
        e.execute(&sel).unwrap();
        sv.push(t.elapsed().as_secs_f64() * 1000.0);
        let t = Instant::now();
        e.execute(&upd).unwrap();
        uv.push(t.elapsed().as_secs_f64() * 1000.0);
        let t = Instant::now();
        e.execute(&upd_eq).unwrap();
        uev.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    (
        median(rv),
        median(ev),
        median(r1v),
        median(sv),
        median(uv),
        median(uev),
    )
}

fn stat(e: &mut Engine, col: &str) -> i64 {
    match e
        .execute(&format!(
            "SELECT {col} FROM pg_stat_user_tables WHERE relname='wb'"
        ))
        .unwrap()
    {
        spg_engine::QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0])
            .parse()
            .unwrap_or(-1),
        _ => -1,
    }
}

/// The engine already counts index scans (`note_index_scan` ->
/// `pg_stat_user_tables.idx_scan`), so whether a seek fired is observable
/// without adding anything: run one statement, read the delta.
fn seek_fired(total: i64) {
    let mut e = Engine::new();
    e.execute("CREATE TABLE wb(id INT PRIMARY KEY, g INT, v INT)")
        .unwrap();
    for chunk in 0..(total / 1000) {
        e.execute(&batch_sql(chunk * 1000, 1000)).unwrap();
    }
    let seg = total / 2;
    let cases: Vec<(&str, String)> = vec![
        (
            "SELECT range",
            format!("SELECT count(*) FROM wb WHERE id >= {seg} AND id < {}", seg + 1),
        ),
        (
            "UPDATE range",
            format!("UPDATE wb SET v = v WHERE id >= {seg} AND id < {}", seg + 1),
        ),
        (
            "DELETE range",
            format!("DELETE FROM wb WHERE id >= {seg} AND id < {}", seg + 1),
        ),
        (
            "DELETE equality",
            format!("DELETE FROM wb WHERE id = {}", seg + 5),
        ),
        // Control: `g` carries no index, so this one MUST scan. If
        // seq_tup_read stays 0 here, the counter is not wired for the
        // mutation path and "no scan" cannot be read off it.
        (
            "DELETE unindexed",
            "DELETE FROM wb WHERE g = 999999".to_string(),
        ),
    ];
    println!("# seek behaviour on a {total}-row table (each predicate matches ONE row)");
    println!("  statement          idx_scan  rows fetched by the seek  rows read");
    for (label, sql) in cases {
        let (s0, f0, r0) = (
            stat(&mut e, "idx_scan"),
            stat(&mut e, "idx_tup_fetch"),
            stat(&mut e, "seq_tup_read"),
        );
        e.execute(&sql).unwrap();
        let (s1, f1, r1) = (
            stat(&mut e, "idx_scan"),
            stat(&mut e, "idx_tup_fetch"),
            stat(&mut e, "seq_tup_read"),
        );
        println!(
            "  {label:<16}   +{:<6}  {:>22}  {:>9}",
            s1 - s0,
            f1 - f0,
            r1 - r0
        );
    }
}

fn main() {
    seek_fired(50_000);
    println!();
    println!("# DELETE cost vs table size (embedded), median of 21");
    println!("# all range predicates below match exactly ONE row");
    println!(
        "| table rows | DEL range | UPD range | DEL equality | UPD equality | SEL range |"
    );
    println!(
        "|-----------:|----------:|----------:|-------------:|-------------:|----------:|"
    );
    for total in [10_000i64, 50_000, 200_000] {
        let (_r, eq, r1, sel, upd, upd_eq) = run(total);
        println!(
            "| {total:10} | {r1:6.3} ms | {upd:6.3} ms | {eq:9.3} ms | {upd_eq:9.3} ms | {sel:6.3} ms |"
        );
    }
}
