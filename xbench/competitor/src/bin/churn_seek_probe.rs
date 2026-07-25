//! P0-23 — what does the index seek return once a table has been churned?
//!
//! Round 459 found a 12x knee: delete-and-reinsert the same 1000 rows and the
//! per-DELETE cost jumps from 0.33 ms to 4.7 ms somewhere between 20 and 50
//! cycles, then plateaus, while PG18 stays at 0.3 ms. The named-thread
//! profile of the slow state is dominated by per-row `eval_expr`, and the
//! caller re-evaluates the full WHERE once per candidate the seek hands
//! back — so the question is how many candidates that is.
use spg_engine::{Engine, QueryResult};
use std::fmt::Write as _;
use std::time::Instant;

const TOTAL: i64 = 50_000;

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

fn stat(e: &mut Engine, col: &str) -> i64 {
    match e
        .execute(&format!(
            "SELECT {col} FROM pg_stat_user_tables WHERE relname='wb'"
        ))
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0])
            .parse()
            .unwrap_or(-1),
        other => panic!("{other:?}"),
    }
}

fn main() {
    // The embedded engine runs autovacuum INLINE at statement exit, which
    // caps dead rows around 12.5k on a 50k table. The server flips that off
    // and drives a background worker instead (PG's shape). `SPG_NO_AV=1`
    // reproduces the server's exposure: if the worker does not keep up, dead
    // rows are unbounded, and the seek hands back one candidate per dead
    // version — each of which pays a full WHERE re-evaluation.
    let no_av = std::env::var("SPG_NO_AV").is_ok_and(|v| v != "0");
    let mut e = Engine::new();
    if no_av {
        e.set_autovacuum(false);
    }
    println!("# autovacuum = {}", !no_av);
    e.execute("CREATE TABLE wb(id INT PRIMARY KEY, g INT, v INT)")
        .unwrap();
    for chunk in 0..(TOTAL / 1000) {
        e.execute(&batch_sql(chunk * 1000, 1000)).unwrap();
    }
    let seg = TOTAL / 2;
    let del = format!("DELETE FROM wb WHERE id >= {seg} AND id < {}", seg + 1000);
    let ins = batch_sql(seg, 1000);

    println!("# churn cycles vs what one DELETE costs and touches");
    println!("| cycle | DELETE ms | seek fetched | seq rows read | dead |");
    println!("|------:|----------:|-------------:|--------------:|-----:|");
    for cycle in 0..=60 {
        let f0 = stat(&mut e, "idx_tup_fetch");
        let r0 = stat(&mut e, "seq_tup_read");
        let t = Instant::now();
        e.execute(&del).unwrap();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let f1 = stat(&mut e, "idx_tup_fetch");
        let r1 = stat(&mut e, "seq_tup_read");
        let dead = stat(&mut e, "n_dead_tup");
        if cycle % 10 == 0 || cycle == 60 {
            println!(
                "| {cycle:5} | {ms:9.3} | {:12} | {:13} | {dead:4} |",
                f1 - f0,
                r1 - r0
            );
        }
        e.execute(&ins).unwrap();
    }
}
