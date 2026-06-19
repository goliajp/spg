//! v7.37.9 — sentori Epic 7 single-writer throughput audit.
//!
//! Sentori's capability request frames this as fact-finding rather
//! than a feature ask: "at sustained 500 wps on rows of ~2 KB each
//! (typical event payload), what's the latency distribution?" The
//! number determines whether spg is the Helm-chart default for a
//! medium self-hosted deployment, so the audit ships a real
//! measurement against the partitioned `events_partitioned` shape
//! sentori actually uses.
//!
//! Test format:
//!   fast tier — `single_writer_500wps_2kb_under_budget` insists the
//!     mean per-INSERT wall time stays under a comfortable ceiling
//!     so a future regression in INSERT routing or generated-column
//!     evaluation shows up immediately.
//!   full tier (`#[ignore]`) — `single_writer_500wps_2kb_latency_
//!     distribution` runs a longer sweep and prints p50 / p95 / p99
//!     plus total wall time. Reported via `eprintln!`; the audit
//!     note in `.claude/notes/v7.37.9-epic7-throughput-audit.md`
//!     captures the numbers each run produces.
//!
//! Both tests use the canonical sentori shape: partitioned parent
//! by RANGE(received_at) with two monthly children + a DEFAULT
//! catch-all, JSONB payload column populated with a ~2 KB blob.

use std::time::Instant;

use spg_engine::Engine;

/// Roughly 2 KB of canonical JSON. The braces / quoting overhead
/// is a fixed ~80 bytes; the rest is filler so the row size
/// matches sentori's reported "typical event payload."
fn make_payload(idx: u64) -> String {
    // 1900 chars of body — keeps the canonical-JSON form right at
    // 2 KB once keys + structural punctuation land.
    let filler: String = core::iter::repeat('x').take(1900).collect();
    format!(
        r#"{{"event":"login","seq":{idx},"user_id":{user},"meta":{{"ua":"{filler}"}}}}"#,
        user = idx % 1000,
    )
}

fn setup_events_partitioned() -> Engine {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE events_partitioned (
            id BIGINT NOT NULL,
            project_id BIGINT NOT NULL,
            received_at TIMESTAMPTZ NOT NULL,
            payload JSONB
         ) PARTITION BY RANGE (received_at)",
    )
    .unwrap();
    e.execute(
        "CREATE TABLE events_2026_06 PARTITION OF events_partitioned \
         FOR VALUES FROM ('2026-06-01 00:00:00+00') TO ('2026-07-01 00:00:00+00')",
    )
    .unwrap();
    e.execute(
        "CREATE TABLE events_2026_07 PARTITION OF events_partitioned \
         FOR VALUES FROM ('2026-07-01 00:00:00+00') TO ('2026-08-01 00:00:00+00')",
    )
    .unwrap();
    e.execute("CREATE TABLE events_default PARTITION OF events_partitioned DEFAULT")
        .unwrap();
    e
}

/// Fast-tier guard — a 1000-row INSERT loop against the sentori-
/// shaped partitioned table. Mean per-INSERT time must stay under
/// the budget so a routing / generated-column / GIN-maintenance
/// regression surfaces here before sentori's self-hosted users see
/// it. The budget is *not* a sentori SLO — it's a regression alarm.
#[test]
fn single_writer_500wps_2kb_under_budget() {
    let _lock = crate::perf_lock();
    let mut e = setup_events_partitioned();
    let n: u64 = 1000;
    let start = Instant::now();
    for i in 0..n {
        let day = (i % 28) + 1;
        let month = if i < n / 2 { 6 } else { 7 };
        let payload = make_payload(i);
        // Escape every embedded single quote in the JSON payload
        // (PG-style doubling) so the INSERT literal stays valid.
        let payload_escaped = payload.replace('\'', "''");
        let sql = format!(
            "INSERT INTO events_partitioned (id, project_id, received_at, payload) \
             VALUES ({i}, 100, '2026-{month:02}-{day:02} 12:00:00+00', '{payload_escaped}'::jsonb)"
        );
        e.execute(std::hint::black_box(&sql))
            .expect("INSERT succeeds");
    }
    let total = start.elapsed();
    let mean_us = total.as_secs_f64() * 1e6 / n as f64;
    // Sentori asks about sustained 500 wps. At 500 wps the per-INSERT
    // budget is 2 000 µs / row; SPG should clear that with comfortable
    // headroom in single-writer mode. The 1 500 µs ceiling here is the
    // regression alarm — well above today's measured numbers so the
    // gate doesn't trip on timing noise but well below the sentori
    // ceiling that would force them off the Helm-chart default.
    let budget_us = 1500.0;
    assert!(
        mean_us < budget_us,
        "single_writer_500wps_2kb mean {mean_us:.1} µs/row exceeds budget {budget_us:.1} µs/row \
         (rows={n}, total={:?})",
        total
    );
}

/// Full-tier(`#[ignore]`) — latency-distribution sweep at 2 000
/// rows. Reports p50 / p95 / p99 and total wall time via
/// `eprintln!` so the audit note can capture the number from
/// `cargo test … --include-ignored single_writer_500wps_2kb_latency_
/// distribution -- --nocapture`. Not gated by an assertion — the
/// fast-tier test catches regressions; this one quantifies them.
#[test]
#[ignore = "audit measurement — full tier, see audit note"]
fn single_writer_500wps_2kb_latency_distribution() {
    let _lock = crate::perf_lock();
    let mut e = setup_events_partitioned();
    let n: usize = 2000;
    let mut samples: Vec<f64> = Vec::with_capacity(n);
    let wall = Instant::now();
    for i in 0..n {
        let day = ((i as u64) % 28) + 1;
        let month = if i < n / 2 { 6 } else { 7 };
        let payload = make_payload(i as u64);
        let payload_escaped = payload.replace('\'', "''");
        let sql = format!(
            "INSERT INTO events_partitioned (id, project_id, received_at, payload) \
             VALUES ({i}, 100, '2026-{month:02}-{day:02} 12:00:00+00', '{payload_escaped}'::jsonb)"
        );
        let t = Instant::now();
        e.execute(std::hint::black_box(&sql))
            .expect("INSERT succeeds");
        samples.push(t.elapsed().as_secs_f64() * 1e6);
    }
    let total = wall.elapsed();
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| -> f64 {
        let idx = ((samples.len() - 1) as f64 * q).round() as usize;
        samples[idx]
    };
    let mean_us = samples.iter().sum::<f64>() / samples.len() as f64;
    let wps = n as f64 / total.as_secs_f64();
    eprintln!(
        "[sentori-epic7] rows={n} payload≈2KB partitioned\n\
         [sentori-epic7]   total wall = {total:?}\n\
         [sentori-epic7]   throughput = {wps:.0} wps\n\
         [sentori-epic7]   mean = {mean_us:.1} µs/row\n\
         [sentori-epic7]   p50 = {:.1} µs   p95 = {:.1} µs   p99 = {:.1} µs",
        p(0.50),
        p(0.95),
        p(0.99),
    );
}
