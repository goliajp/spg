//! v7.39 (round 498) — session-local state, differentially, across two
//! connections.
//!
//! Round 497 found `currval` answering in a session that had never called
//! `nextval` — session-local state kept globally. That is the same family
//! as rounds 279 and 283 (one shared `Engine`, per-connection state that
//! has to be switched), and the way it was found — a two-connection probe
//! — is the only way to find it. So this sweeps the rest of the surface
//! PG defines as per-session.
//!
//!   ISO_URL=postgres://…  cargo run --release --bin iso_session

use sqlx::{AnyConnection, Connection as _, Executor as _, Row as _};

async fn cell(c: &mut AnyConnection, sql: &str) -> String {
    match c.fetch_all(sql).await {
        Ok(rows) => rows
            .first()
            .and_then(|r| {
                r.try_get::<String, _>(0)
                    .or_else(|_| r.try_get::<i64, _>(0).map(|v| v.to_string()))
                    .or_else(|_| r.try_get::<i32, _>(0).map(|v| v.to_string()))
                    .or_else(|_| r.try_get::<bool, _>(0).map(|v| v.to_string()))
                    .ok()
            })
            .unwrap_or_else(|| "<none>".into()),
        Err(e) => {
            let s = e.to_string();
            format!("ERR({})", s.split(':').next_back().unwrap_or("?").trim())
        }
    }
}

async fn run(c: &mut AnyConnection, sql: &str) -> bool {
    c.execute(sql).await.is_ok()
}

#[allow(clippy::too_many_lines)] // one self-contained scenario per block
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();
    let url = std::env::var("ISO_URL").map_err(|_| "set ISO_URL")?;
    let mut a = AnyConnection::connect(&url).await?;
    let mut b = AnyConnection::connect(&url).await?;

    println!("| # | scenario | expected (PG18) | A | B |");
    println!("|---|----------|-----------------|---|---|");

    // T1 — currval is defined only in the session that called nextval.
    run(&mut a, "DROP SEQUENCE IF EXISTS iso498seq").await;
    run(&mut a, "CREATE SEQUENCE iso498seq").await;
    let _ = cell(&mut a, "SELECT nextval('iso498seq')").await;
    println!(
        "| T1 | A nextval then currval; B currval | 1 / error | {} | {} |",
        cell(&mut a, "SELECT currval('iso498seq')").await,
        cell(&mut b, "SELECT currval('iso498seq')").await
    );

    // T2 — lastval, same rule, no argument.
    println!(
        "| T2 | lastval in A / in B | 1 / error | {} | {} |",
        cell(&mut a, "SELECT lastval()").await,
        cell(&mut b, "SELECT lastval()").await
    );

    // T3 — a GUC set in one session is not seen in the other.
    run(&mut a, "SET application_name = 'only_a'").await;
    println!(
        "| T3 | application_name after A sets it | only_a / '' | {} | {} |",
        cell(&mut a, "SHOW application_name").await,
        cell(&mut b, "SHOW application_name").await
    );

    // T4 — a temporary table belongs to its session.
    run(&mut a, "CREATE TEMP TABLE iso498tmp (x INT)").await;
    run(&mut a, "INSERT INTO iso498tmp VALUES (1)").await;
    println!(
        "| T4 | A's temp table read by A / by B | 1 / error | {} | {} |",
        cell(&mut a, "SELECT count(*)::int FROM iso498tmp").await,
        cell(&mut b, "SELECT count(*)::int FROM iso498tmp").await
    );

    // T5 — a prepared statement belongs to its session.
    run(&mut a, "PREPARE iso498p AS SELECT 42").await;
    println!(
        "| T5 | EXECUTE iso498p in A / in B | 42 / error | {} | {} |",
        cell(&mut a, "EXECUTE iso498p").await,
        cell(&mut b, "EXECUTE iso498p").await
    );

    // T6 — a session-level advisory lock is exclusive across sessions.
    println!(
        "| T6a | pg_backend_pid (must differ) | differ | {} | {} |",
        cell(&mut a, "SELECT pg_backend_pid()").await,
        cell(&mut b, "SELECT pg_backend_pid()").await
    );
    let a_lock = cell(&mut a, "SELECT pg_try_advisory_lock(4981)").await;
    let b_lock = cell(&mut b, "SELECT pg_try_advisory_lock(4981)").await;
    println!("| T6 | pg_try_advisory_lock(4981) A then B | true / false | {a_lock} | {b_lock} |");
    let b2 = cell(&mut b, "SELECT pg_try_advisory_lock(4981)").await;
    let b_unlock = cell(&mut b, "SELECT pg_advisory_unlock(4981)").await;
    println!(
        "| T6b | B tries again / B unlocks a lock it never took | false / false | {b2} | {b_unlock} |"
    );
    let _ = cell(&mut a, "SELECT pg_advisory_unlock(4981)").await;

    // T6c — the same question with both sessions inside a transaction,
    // which takes the &mut executor regardless of the read fast path. If
    // this differs from T6, the defect is the ROUTING; if it matches, the
    // registry itself is not being reached.
    run(&mut a, "BEGIN").await;
    run(&mut b, "BEGIN").await;
    let a_tx = cell(&mut a, "SELECT pg_try_advisory_lock(4982)").await;
    let b_tx = cell(&mut b, "SELECT pg_try_advisory_lock(4982)").await;
    println!("| T6c | same, inside BEGIN on both | true / false | {a_tx} | {b_tx} |");
    run(&mut a, "ROLLBACK").await;
    run(&mut b, "ROLLBACK").await;

    // T7 — a MySQL-style user variable, if the dialect offers one, is
    // per-session too. PG has no such thing, so it errors on both.
    run(&mut a, "SET @uv = 7").await;
    println!(
        "| T7 | @uv in A / in B | error / error on PG | {} | {} |",
        cell(&mut a, "SELECT @uv").await,
        cell(&mut b, "SELECT @uv").await
    );

    // T8 — a sequence's counter IS shared, unlike currval. Round 497's
    // fix; here as the counterpart so the two are not confused.
    let a_next = cell(&mut a, "SELECT nextval('iso498seq')").await;
    let b_next = cell(&mut b, "SELECT nextval('iso498seq')").await;
    println!("| T8 | nextval in A then B (shared counter) | 2 / 3 | {a_next} | {b_next} |");
    Ok(())
}
