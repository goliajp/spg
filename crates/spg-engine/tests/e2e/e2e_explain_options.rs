//! v7.37.22 (22.7) — EXPLAIN BUFFERS / TIMING / SETTINGS / WAL
//! option family. PG's standard surface; dashboards / regression
//! tools depend on the option keywords being accepted (whether
//! or not SPG's internals can produce a meaningful number for
//! each one). Where SPG can fill the number in (BUFFERS hot rows,
//! WAL records=0 for read-only SELECT) it does; the rest stay
//! shaped but inert.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn plan_text(e: &mut Engine, sql: &str) -> String {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    let mut out = String::new();
    for row in &rows {
        if let Value::Text(s) = &row.values[0] {
            out.push_str(s);
            out.push('\n');
        }
    }
    out
}

#[test]
fn explain_buffers_includes_cache_hit_ratio() {
    // v7.37.19 (19.23 [PG+]) — BUFFERS line carries
    // cache_hit_ratio next to hot_rows / cold_rows.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1), (2), (3), (4)")
        .unwrap();
    let plan = plan_text(&mut e, "EXPLAIN (ANALYZE, BUFFERS) SELECT * FROM t");
    assert!(
        plan.contains("cache_hit_ratio="),
        "missing cache_hit_ratio: {plan}"
    );
    // 4 hot, 0 cold → ratio = 100.00.
    assert!(
        plan.contains("cache_hit_ratio=100.00"),
        "ratio for 4 hot / 0 cold should be 100.00: {plan}"
    );
}

#[test]
fn explain_buffers_cache_hit_ratio_is_na_on_empty_result() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    // Empty table — 0 hot + 0 cold → ratio = n/a.
    let plan = plan_text(&mut e, "EXPLAIN (ANALYZE, BUFFERS) SELECT * FROM t");
    assert!(
        plan.contains("cache_hit_ratio=n/a"),
        "empty-result ratio should be n/a: {plan}"
    );
}

#[test]
fn explain_buffers_emits_hot_cold_line() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
    let plan = plan_text(&mut e, "EXPLAIN (ANALYZE, BUFFERS) SELECT * FROM t");
    assert!(
        plan.contains("Buffers: hot_rows=3 cold_rows=0"),
        "missing Buffers line: {plan}"
    );
}

#[test]
fn explain_timing_off_strips_elapsed() {
    // Engine::new() runs without a wall-clock, so elapsed never
    // appears regardless of TIMING. The negative case (TIMING
    // OFF still strips even if the engine has a clock) is what
    // matters for diff-friendly regression output — verify the
    // plan body parses and emits the same skeleton both ways.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    let plan_off = plan_text(&mut e, "EXPLAIN (ANALYZE, TIMING OFF) SELECT * FROM t");
    assert!(
        !plan_off.contains("elapsed="),
        "TIMING OFF must strip elapsed: {plan_off}"
    );
    // TIMING ON parses through without error; the engine's
    // clock guards whether elapsed actually lands. (The
    // clock-bound branch is covered by e2e_explain_analyze.)
    let plan_on = plan_text(&mut e, "EXPLAIN (ANALYZE, TIMING ON) SELECT * FROM t");
    // v7.39 (round 227) — PG shape: the node carries the measured block.
    assert!(
        plan_on.contains("loops=1"),
        "TIMING ON plan body broken: {plan_on}"
    );
}

#[test]
fn explain_settings_emits_overrides_or_placeholder() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    let plan = plan_text(&mut e, "EXPLAIN (SETTINGS) SELECT * FROM t");
    assert!(plan.contains("Settings:"), "missing Settings line: {plan}");
}

#[test]
fn explain_wal_emits_zero_for_read_only_select() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    let plan = plan_text(&mut e, "EXPLAIN (ANALYZE, WAL) SELECT * FROM t");
    assert!(
        plan.contains("WAL: records=0 bytes=0 fpi=0"),
        "missing WAL line: {plan}"
    );
}

#[test]
fn explain_combined_options_compose() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1), (2)").unwrap();
    let plan = plan_text(
        &mut e,
        "EXPLAIN (ANALYZE, BUFFERS, WAL, SETTINGS, TIMING OFF) SELECT * FROM t",
    );
    assert!(plan.contains("Buffers:"), "missing Buffers: {plan}");
    assert!(plan.contains("WAL:"), "missing WAL: {plan}");
    assert!(plan.contains("Settings:"), "missing Settings: {plan}");
    assert!(!plan.contains("elapsed="), "TIMING OFF wins: {plan}");
}

// v7.37.23 (23.5) — EXPLAIN (FORMAT json / xml / yaml).

#[test]
fn explain_format_json_emits_pg_plan_object() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    let plan = plan_text(&mut e, "EXPLAIN (FORMAT json) SELECT * FROM t");
    let plan = plan.trim_end();
    assert!(plan.starts_with('['), "JSON must open with [: {plan}");
    assert!(plan.ends_with(']'), "JSON must close with ]: {plan}");
    // v7.39 (round 226) — PG's nested node objects replaced the old
    // per-line `{"Plan Line": …}` fallback.
    assert!(plan.contains("\"Plan\""), "PG Plan wrapper missing: {plan}");
    assert!(
        plan.contains("\"Node Type\": \"Seq Scan\""),
        "PG Node Type missing: {plan}"
    );
    assert!(
        plan.contains("\"Relation Name\": \"t\""),
        "relation name missing: {plan}"
    );
}

#[test]
fn explain_format_xml_wraps_plan_in_explain_element() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    let plan = plan_text(&mut e, "EXPLAIN (FORMAT xml) SELECT * FROM t");
    let plan = plan.trim_end();
    assert!(
        plan.starts_with("<explain"),
        "XML must open with <explain: {plan}"
    );
    assert!(plan.ends_with("</explain>"), "XML must close: {plan}");
    assert!(plan.contains("<line>"), "line element missing: {plan}");
}

#[test]
fn explain_format_yaml_emits_list_items() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    let plan = plan_text(&mut e, "EXPLAIN (FORMAT yaml) SELECT * FROM t");
    assert!(
        plan.starts_with("- Plan:"),
        "YAML must start with `- Plan:`: {plan}"
    );
    assert!(plan.contains("  - "), "no nested list item: {plan}");
}

#[test]
fn explain_format_text_default_unchanged() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    // v7.39 (round 224) — a filtered query so the PG-shaped plan has a
    // Filter attr line (a bare scan is one line).
    let plan = plan_text(&mut e, "EXPLAIN (FORMAT text) SELECT * FROM t WHERE id = 1 ORDER BY id");
    assert!(
        plan.contains("Seq Scan on t"),
        "TEXT format body broken: {plan}"
    );
    // TEXT format emits one row per line; JSON would be a single
    // row whose body starts with `[`. Confirm we have multiple
    // lines (i.e. multiple rows joined by `\n` in our plan_text
    // helper).
    assert!(
        plan.matches('\n').count() >= 2,
        "TEXT format must emit multiple rows: {plan}"
    );
}

#[test]
fn explain_format_unknown_rejected() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    let err = e
        .execute("EXPLAIN (FORMAT lisp) SELECT * FROM t")
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("FORMAT") || msg.contains("lisp"),
        "expected unknown-format error: {msg}"
    );
}

#[test]
fn explain_accepts_pg_format_and_verbose_aliases() {
    // VERBOSE, FORMAT text, SUMMARY are accepted but treated as
    // no-ops so EXPLAIN-using clients (pgAdmin, DataGrip) don't
    // see syntax errors.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("EXPLAIN (VERBOSE) SELECT * FROM t").unwrap();
    e.execute("EXPLAIN (FORMAT text) SELECT * FROM t").unwrap();
    e.execute("EXPLAIN (ANALYZE, SUMMARY ON) SELECT * FROM t")
        .unwrap();
}
