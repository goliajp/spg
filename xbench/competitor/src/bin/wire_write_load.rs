//! v7.39 (round 491) — a wire write load that outlives a sampler.
//!
//! Both surviving write losses make the same claim: the engine runs the
//! statement far faster than the wire round trip, so the cost is the
//! server path between the socket and the engine. Round 450 measured that
//! gap at 1.29-1.38x on the execution path (`synchronous_commit=off`);
//! rounds 446-449 counted and excluded the engine, the commit queue, WAL
//! preallocation, the fsync syscall, churn, and the AST deep clone. What
//! is left has never been profiled: round 457's probe finished before the
//! sampler attached, and round 459 found the server's connection threads
//! had no names to attribute samples to (since fixed).
//!
//! The shape comes from `write_shapes::run_shape` — the panel's own
//! definition — rather than a transcription of it. Rounds 447 and 448 both
//! measured a shape that differed from the panel's in a way that changed
//! the answer, so this borrows instead of restating.
//!
//!   SPG_WIRE_URL=postgres://…  SPG_PROBE_SHAPE=insert_batch_1k \
//!   SPG_PROBE_ITERS=2500  cargo run --release --bin wire_write_load

use spg_bench_competitor::write_shapes::{N, SHAPES, batch_insert_sql, median_ms, run_shape};
use sqlx::{AnyConnection, Connection as _, Executor as _};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();
    let url = std::env::var("SPG_WIRE_URL").map_err(|_| "set SPG_WIRE_URL")?;
    let want = std::env::var("SPG_PROBE_SHAPE").unwrap_or_else(|_| "insert_batch_1k".into());
    let iters: usize = std::env::var("SPG_PROBE_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    let (_, shape) = SHAPES
        .iter()
        .find(|(n, _)| *n == want)
        .ok_or("unknown SPG_PROBE_SHAPE")?;

    let mut c = AnyConnection::connect(&url).await?;
    c.execute("DROP TABLE IF EXISTS wb").await?;
    c.execute("CREATE TABLE wb(id INT PRIMARY KEY, g INT, v INT)")
        .await?;
    for chunk in 0..(N / 1000) {
        c.execute(batch_insert_sql(chunk * 1000 + 1, 1000).as_str())
            .await?;
    }
    // The panel's verdict column reads the execution path: durability costs
    // both engines the same (round 450) and only adds variance here.
    c.execute("SET synchronous_commit = off").await?;

    // `run_shape` wants a blocking closure; the connection is async, so the
    // statements go through a small bridge on the current runtime handle.
    let handle = tokio::runtime::Handle::current();
    let mut samples = Vec::with_capacity(iters);
    let mut next_base = N + 1_000_000;
    let mut exec = |sql: &str| {
        tokio::task::block_in_place(|| {
            handle
                .block_on(c.execute(sql))
                .unwrap_or_else(|e| panic!("{sql:.80}: {e}"));
        });
    };
    for _ in 0..iters {
        samples.push(run_shape(*shape, next_base, &mut exec));
        next_base += 10_000;
    }
    println!("{want}: {iters} iterations, median {:.3} ms", median_ms(samples));
    Ok(())
}
