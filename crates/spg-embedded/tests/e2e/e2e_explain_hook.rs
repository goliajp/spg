//! v7.36 (mailrs ask #4) — programmatic `Database::explain` hook.
//! Returns the QUERY PLAN lines as `Vec<String>` so dogfood
//! callers can attach the plan to a report or assert on its shape
//! from a test.

use spg_embedded::Database;

#[test]
fn explain_returns_query_plan_lines() {
    let mut db = Database::open_in_memory();
    db.execute(
        "CREATE TABLE messages (id BIGINT NOT NULL PRIMARY KEY, mailbox_id BIGINT NOT NULL)",
    )
    .unwrap();
    db.execute("CREATE TABLE mailboxes (id BIGINT NOT NULL PRIMARY KEY, name TEXT NOT NULL)")
        .unwrap();
    let plan = db
        .explain(
            "SELECT m.id FROM messages m \
             JOIN mailboxes mb ON m.mailbox_id = mb.id \
             ORDER BY m.id DESC LIMIT 200",
        )
        .expect("explain must succeed on a plain SELECT");
    assert!(
        !plan.is_empty(),
        "EXPLAIN must produce at least one QUERY PLAN line"
    );
    // First line names the top operator — for this shape it should
    // mention a JOIN or a scan over messages.
    let head = &plan[0];
    assert!(
        head.contains("messages") || head.contains("Join") || head.contains("Scan"),
        "expected top line to name messages / Join / Scan, got: {head}"
    );
    // Some line mentions ORDER BY or LIMIT 200 (planner walker
    // path captures the LIMIT).
    let has_limit_or_order = plan
        .iter()
        .any(|l| l.contains("Limit") || l.contains("OrderBy"));
    assert!(
        has_limit_or_order,
        "expected OrderBy or Limit mention in plan, got: {plan:?}"
    );
}

#[test]
fn explain_propagates_parse_error() {
    let db = Database::open_in_memory();
    // Bare syntax error — must surface, not return an empty Vec.
    let err = db
        .explain("CRA ZY NOT SQL")
        .expect_err("syntax error must propagate as an EngineError");
    let msg = format!("{err}");
    assert!(!msg.is_empty(), "expected non-empty error message");
}
