//! v7.37.9 — sentori cutover acceptance, end-to-end.
//!
//! Exercises the full sentori migration shape across every v7.37
//! capability now in tree, in one walk-through that mimics the
//! sentori app's real ingest → query → retention flow:
//!   * partitioned `events_partitioned` (Epic 2)
//!   * partitioned `spans` (Epic 2 again — high-volume tracing)
//!   * `issues` table with stored generated `search_vector`
//!     (Epic 3)
//!   * `issues.labels` JSONB column + GIN index (Epic 5)
//!   * `endpoint_check` + `endpoint_probe` + LATERAL probe query
//!     (Epic 4)
//!   * JSONB operator usage across the suite (Epic 6 — `->`, `->>`,
//!     `@>`, `jsonb_set`)
//!
//! Each test pins one slice of the migration so a regression in any
//! prior epic surfaces as a named failure rather than a vague
//! "sentori broke." The probe is intentionally heavier than the
//! per-epic e2e files so a v7.37.x ship blocker that only manifests
//! at the integration boundary still has a guard.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<Value>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows for {sql}");
    };
    rows.into_iter().map(|r| r.values).collect()
}

fn one_i64(e: &mut Engine, sql: &str) -> i64 {
    let mut rs = rows(e, sql);
    let row = rs.pop().expect("one row");
    match row.into_iter().next().expect("one col") {
        Value::BigInt(n) => n,
        Value::Int(n) => i64::from(n),
        other => panic!("expected integer, got {other:?}"),
    }
}

/// Sentori migration `server/migrations/0003_partition_events.sql`
/// shape — partition by RANGE on TIMESTAMPTZ, two monthly children,
/// plus a DEFAULT catch-all for out-of-window writes (clock skew).
fn setup_events_partitioned(e: &mut Engine) {
    e.execute(
        "CREATE TABLE events_partitioned (
            id BIGINT NOT NULL,
            project_id BIGINT NOT NULL,
            received_at TIMESTAMPTZ NOT NULL,
            payload JSONB
         ) PARTITION BY RANGE (received_at)",
    )
    .expect("CREATE events_partitioned parent");
    e.execute(
        "CREATE TABLE events_2026_06 PARTITION OF events_partitioned \
         FOR VALUES FROM ('2026-06-01 00:00:00+00') TO ('2026-07-01 00:00:00+00')",
    )
    .expect("CREATE June child");
    e.execute(
        "CREATE TABLE events_2026_07 PARTITION OF events_partitioned \
         FOR VALUES FROM ('2026-07-01 00:00:00+00') TO ('2026-08-01 00:00:00+00')",
    )
    .expect("CREATE July child");
    e.execute("CREATE TABLE events_default PARTITION OF events_partitioned DEFAULT")
        .expect("CREATE DEFAULT child");
}

/// Sentori migration `server/migrations/0034_issues_fulltext.sql`
/// shape — issues table carries a JSONB labels column + a tsvector
/// generated-stored column that materialises from title + body.
fn setup_issues(e: &mut Engine) {
    e.execute(
        "CREATE TABLE issues (
            id BIGINT NOT NULL,
            title TEXT,
            body TEXT,
            labels JSONB,
            search_vector TSVECTOR GENERATED ALWAYS AS (
                to_tsvector('simple', coalesce(title, '') || ' ' || coalesce(body, ''))
            ) STORED
         )",
    )
    .expect("CREATE issues with generated search_vector");
    e.execute("CREATE INDEX issues_labels_gin ON issues USING GIN (labels)")
        .expect("CREATE issues_labels_gin (real JSONB-GIN)");
}

/// Sentori migration shape for the endpoint-probe runtime query
/// (Epic 4 LATERAL). `endpoint_check` carries the URL + paused
/// flag; `endpoint_probe` records each probe run with a timestamp.
fn setup_endpoint_tables(e: &mut Engine) {
    e.execute(
        "CREATE TABLE endpoint_check (
            id BIGINT PRIMARY KEY,
            project_id BIGINT NOT NULL,
            url TEXT NOT NULL,
            paused BOOL NOT NULL DEFAULT FALSE
         )",
    )
    .unwrap();
    e.execute(
        "CREATE TABLE endpoint_probe (
            id BIGINT PRIMARY KEY,
            check_id BIGINT NOT NULL,
            ts TIMESTAMPTZ NOT NULL
         )",
    )
    .unwrap();
}

/// Top-level cutover walk-through — every epic working in concert
/// against the real sentori schema and ingest pattern.
#[test]
fn full_sentori_cutover_walk_through() {
    let mut e = Engine::new();
    setup_events_partitioned(&mut e);
    setup_issues(&mut e);
    setup_endpoint_tables(&mut e);

    // === Epic 2 + Epic 6 — ingest into the partitioned parent ===
    // Sentori writes ~166 events/sec average; this seeds three rows
    // spread across two months + one in the DEFAULT bucket. INSERT
    // routes each tuple to the matching child by `received_at`.
    e.execute(
        "INSERT INTO events_partitioned (id, project_id, received_at, payload) VALUES \
           (1, 100, '2026-06-15 00:00:00+00', '{\"event\":\"login\",\"user\":42}'::jsonb), \
           (2, 100, '2026-07-15 00:00:00+00', '{\"event\":\"checkout\",\"user\":7}'::jsonb), \
           (3, 100, '2025-12-01 00:00:00+00', '{\"event\":\"replay_late\"}'::jsonb)",
    )
    .expect("INSERT routes to children");
    // Child-wise counts confirm routing: June = 1, July = 1, DEFAULT = 1.
    assert_eq!(one_i64(&mut e, "SELECT count(*) FROM events_2026_06"), 1);
    assert_eq!(one_i64(&mut e, "SELECT count(*) FROM events_2026_07"), 1);
    assert_eq!(one_i64(&mut e, "SELECT count(*) FROM events_default"), 1);
    // Parent SELECT walks the UNION ALL and sees the full set.
    assert_eq!(
        one_i64(&mut e, "SELECT count(*) FROM events_partitioned"),
        3
    );

    // Epic 6 — `->` / `->>` operator usage against partitioned data.
    let payload_text = rows(
        &mut e,
        "SELECT payload ->> 'event' FROM events_partitioned WHERE id = 1",
    );
    assert_eq!(payload_text.len(), 1);
    assert_eq!(payload_text[0][0], Value::Text("login".to_string()));

    // Planner pruning: WHERE that constrains to June only sees the
    // events_2026_06 child + DEFAULT (DEFAULT contributes zero rows
    // because the row-level filter excludes the December row).
    assert_eq!(
        one_i64(
            &mut e,
            "SELECT count(*) FROM events_partitioned \
             WHERE received_at >= '2026-06-01 00:00:00+00' \
             AND received_at <  '2026-07-01 00:00:00+00'"
        ),
        1
    );

    // === Epic 3 + Epic 5 + Epic 6 — issues ingest ===
    // Two issues with distinct labels + body. The GENERATED column
    // populates from title + body; the GIN index picks up the labels.
    e.execute(
        "INSERT INTO issues (id, title, body, labels) VALUES \
         (1, 'login fails', 'race when token expires', '{\"team\":\"ios\",\"sev\":\"high\"}'::jsonb), \
         (2, 'checkout slow', 'cart total mismatch', '{\"team\":\"web\",\"sev\":\"high\"}'::jsonb)",
    )
    .expect("INSERT issues");

    // Epic 3 — search_vector was computed; CAST to text gives the
    // canonical tsvector form. Both 'race' and 'token' survived
    // tokenisation.
    let rendered = rows(
        &mut e,
        "SELECT CAST(search_vector AS TEXT) FROM issues WHERE id = 1",
    );
    let Value::Text(canon) = &rendered[0][0] else {
        panic!("expected tsvector text rendering");
    };
    assert!(canon.contains("race") && canon.contains("token"));

    // Epic 5 — GIN-accelerated `@>` containment. Both issues carry
    // sev:high, but only one is on the ios team.
    assert_eq!(
        one_i64(
            &mut e,
            "SELECT count(*) FROM issues WHERE labels @> '{\"team\":\"ios\"}'::jsonb"
        ),
        1
    );
    assert_eq!(
        one_i64(
            &mut e,
            "SELECT count(*) FROM issues WHERE labels @> '{\"sev\":\"high\"}'::jsonb"
        ),
        2
    );
    // Both keys must match — narrows back to one.
    assert_eq!(
        one_i64(
            &mut e,
            "SELECT count(*) FROM issues WHERE labels @> \
             '{\"team\":\"ios\",\"sev\":\"high\"}'::jsonb"
        ),
        1
    );

    // === Epic 4 — LATERAL endpoint probe runtime query ===
    e.execute("INSERT INTO endpoint_check VALUES (1, 100, 'https://a', FALSE)")
        .unwrap();
    e.execute("INSERT INTO endpoint_check VALUES (2, 100, 'https://b', FALSE)")
        .unwrap();
    e.execute("INSERT INTO endpoint_probe VALUES (10, 1, '2026-06-01 00:00:00+00')")
        .unwrap();
    e.execute("INSERT INTO endpoint_probe VALUES (11, 1, '2026-06-15 00:00:00+00')")
        .unwrap();
    let probe_result = rows(
        &mut e,
        "SELECT c.id, lp.ts \
         FROM endpoint_check c \
         LEFT JOIN LATERAL ( \
             SELECT ts FROM endpoint_probe \
              WHERE check_id = c.id \
              ORDER BY ts DESC LIMIT 1 \
         ) lp ON TRUE \
         ORDER BY c.id",
    );
    assert_eq!(probe_result.len(), 2);
    // check_id=1 had two probes; latest is 06-15. check_id=2 had
    // none → LEFT JOIN preserves the row with NULL.
    assert!(matches!(probe_result[0][1], Value::Timestamp(_)));
    assert_eq!(probe_result[1][1], Value::Null);

    // === Epic 2 retention — DROP a child partition ===
    // Sentori's `server/src/retention.rs` background task drops
    // child partitions older than the configured window. The
    // parent SELECT then continues to work; rows under the
    // dropped child are gone but the parent UNION ALL re-resolves.
    e.execute("DROP TABLE events_2026_06").unwrap();
    assert_eq!(
        one_i64(&mut e, "SELECT count(*) FROM events_partitioned"),
        2
    );

    // === Epic 6 — jsonb_set on an existing payload ===
    // Sentori mutates payloads via jsonb_set in 4 callsites; pin the
    // canonical shape here so any future regression in the operator
    // family surfaces in the cutover suite too.
    e.execute(
        "INSERT INTO events_partitioned (id, project_id, received_at, payload) VALUES \
           (4, 100, '2026-07-20 00:00:00+00', '{\"event\":\"checkout\",\"meta\":{\"step\":1}}'::jsonb)",
    )
    .unwrap();
    let mutated = rows(
        &mut e,
        "SELECT jsonb_set(payload, '{meta,step}', '99'::jsonb) FROM events_partitioned WHERE id = 4",
    );
    let Value::Json(text) = &mutated[0][0] else {
        panic!("expected jsonb result, got {:?}", mutated[0][0]);
    };
    assert!(text.contains("\"step\":99") || text.contains("\"step\": 99"));
}

/// Throughput-shaped probe — drive the partitioned events table
/// through a 2000-row ingest to exercise INSERT routing at a non-
/// trivial scale + confirm the GIN-on-jsonb index on a side table
/// stays consistent under interleaved writes. Not a perf bench —
/// Epic 7 lives in `xbench/`. Here we only want the correctness
/// guard at the integration boundary.
#[test]
fn sentori_throughput_shape_2k_ingest() {
    let mut e = Engine::new();
    setup_events_partitioned(&mut e);
    e.execute("CREATE TABLE issues (id BIGINT NOT NULL, labels JSONB)")
        .unwrap();
    e.execute("CREATE INDEX issues_labels_gin ON issues USING GIN (labels)")
        .unwrap();
    let teams = ["ios", "android", "web", "rn"];
    // 2000 events alternating month + an issue per event.
    for i in 0..2000i64 {
        let day = (i % 28) + 1;
        let month_idx = (i / 1000) as u32;
        let ts = if month_idx == 0 {
            format!("'2026-06-{day:02} 12:00:00+00'")
        } else {
            format!("'2026-07-{day:02} 12:00:00+00'")
        };
        e.execute(&format!(
            "INSERT INTO events_partitioned (id, project_id, received_at, payload) \
             VALUES ({i}, 100, {ts}, '{{\"event\":\"e{i}\"}}'::jsonb)"
        ))
        .unwrap();
        let team = teams[(i as usize) % teams.len()];
        e.execute(&format!(
            "INSERT INTO issues (id, labels) VALUES ({i}, '{{\"team\":\"{team}\"}}'::jsonb)"
        ))
        .unwrap();
    }
    // Every event landed in its month child (no DEFAULT rows).
    assert_eq!(one_i64(&mut e, "SELECT count(*) FROM events_2026_06"), 1000);
    assert_eq!(one_i64(&mut e, "SELECT count(*) FROM events_2026_07"), 1000);
    assert_eq!(one_i64(&mut e, "SELECT count(*) FROM events_default"), 0);
    assert_eq!(
        one_i64(&mut e, "SELECT count(*) FROM events_partitioned"),
        2000
    );
    // GIN index found every ios-team issue under interleaved writes.
    assert_eq!(
        one_i64(
            &mut e,
            "SELECT count(*) FROM issues WHERE labels @> '{\"team\":\"ios\"}'::jsonb"
        ),
        500
    );
}
