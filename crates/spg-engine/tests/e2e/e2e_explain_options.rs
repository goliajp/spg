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
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
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
    let plan_off = plan_text(
        &mut e,
        "EXPLAIN (ANALYZE, TIMING OFF) SELECT * FROM t",
    );
    assert!(
        !plan_off.contains("elapsed="),
        "TIMING OFF must strip elapsed: {plan_off}"
    );
    // TIMING ON parses through without error; the engine's
    // clock guards whether elapsed actually lands. (The
    // clock-bound branch is covered by e2e_explain_analyze.)
    let plan_on = plan_text(
        &mut e,
        "EXPLAIN (ANALYZE, TIMING ON) SELECT * FROM t",
    );
    assert!(
        plan_on.contains("Total: rows="),
        "TIMING ON plan body broken: {plan_on}"
    );
}

#[test]
fn explain_settings_emits_overrides_or_placeholder() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    let plan = plan_text(&mut e, "EXPLAIN (SETTINGS) SELECT * FROM t");
    assert!(
        plan.contains("Settings:"),
        "missing Settings line: {plan}"
    );
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
