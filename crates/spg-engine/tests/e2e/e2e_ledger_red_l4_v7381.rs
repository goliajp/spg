//! 7.38.1 S0.1 — ledger red pin for L4 (see e2e_ledger_reds_v7381
//! for the pin discipline). In its own file so the full tier's
//! red-pin skip list can retire entries one defect at a time.

use spg_engine::{Engine, QueryResult};

fn one_cell(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

/// L4 (MATRIX #18) — embedded text for timestamptz must carry the PG
/// offset suffix, exactly what the wire already sends (live PG18:
/// `2026-01-05 09:00:00+00`). Today `value_to_text` drops it.
#[test]
#[ignore = "7.38.1 L4 red (MATRIX #18) — un-ignore in S4.1"]
fn l4_timestamptz_text_carries_the_pg_offset() {
    let mut e = Engine::new().with_clock(|| 0);
    assert_eq!(
        one_cell(&mut e, "SELECT '2026-01-05 09:00:00+00'::timestamptz"),
        "2026-01-05 09:00:00+00",
    );
}
