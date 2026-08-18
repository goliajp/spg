//! 7.38.1 S0.1 — ledger pin for L4 (MATRIX #18), red until S4.1.
//! In its own file so the full tier's red-pin skip list could retire
//! entries one defect at a time; green as of S4.1 (D5).

use spg_engine::{Engine, QueryResult};

/// L4 (MATRIX #18) — embedded text for timestamptz must carry the PG
/// offset suffix, exactly what the wire already sends (live PG18:
/// `2026-01-05 09:00:00+00`). tz-ness lives in the COLUMN (storage is
/// the same UTC i64 as timestamp), so the canonical embedded render
/// goes through the column-aware `value_to_text_typed` — the same
/// type-addressed shape as PG's out-functions.
#[test]
fn l4_timestamptz_text_carries_the_pg_offset() {
    let mut e = Engine::new().with_clock(|| 0);
    match e
        .execute("SELECT '2026-01-05 09:00:00+00'::timestamptz")
        .unwrap()
    {
        QueryResult::Rows { columns, rows } => {
            assert!(
                matches!(columns[0].ty, spg_storage::DataType::Timestamptz),
                "the cast must type its result column, got {:?}",
                columns[0].ty
            );
            assert_eq!(
                spg_engine::eval::value_to_text_typed(&rows[0].values[0], &columns[0].ty),
                "2026-01-05 09:00:00+00",
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }
}
