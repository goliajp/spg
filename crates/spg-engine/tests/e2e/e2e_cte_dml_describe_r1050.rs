//! r1050 — sentori report 3: the outer SELECT of a data-modifying CTE
//! describes its columns.
//!
//! `WITH up AS (INSERT … RETURNING id) SELECT up.id, prev.h AS
//! prev_hash …` described as NoData; sqlx sizes rows by Describe, so
//! the row had zero columns and their suite stopped on step 16 of 86.
//! The r1049 fix answered the statement standing alone; this is the
//! same family one level of nesting deeper, and the two now share one
//! resolver (`dml_returning_columns`).
//!
//! They kept the CTE for a reason worth preserving in the pin: `prev`
//! reads the pre-insert snapshot in the same statement, which is what
//! lets the response say whether the bytes are new. The shape is
//! load-bearing, not a flourish.

use spg_engine::Engine;

fn describe(e: &Engine, sql: &str) -> Vec<(String, spg_storage::DataType)> {
    let stmt = spg_sql::parser::parse_statement(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let (_, cols) = e.describe_prepared(&stmt);
    cols.into_iter().map(|c| (c.name, c.ty)).collect()
}

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ra (id UUID PRIMARY KEY, k TEXT, h TEXT)")
        .unwrap();
    e
}

/// The sentori upload shape, name for name and type for type.
#[test]
fn r1050_the_upload_cte_describes_both_columns() {
    let e = engine();
    let cols = describe(
        &e,
        "WITH prev AS (SELECT h FROM ra WHERE k = 'a'), \
              up AS (INSERT INTO ra (id, k, h) VALUES (gen_random_uuid(), 'a', 'new') \
                     ON CONFLICT (k) DO UPDATE SET h = EXCLUDED.h RETURNING id) \
         SELECT up.id, prev.h AS prev_hash FROM up LEFT JOIN prev ON true",
    );
    assert_eq!(
        cols,
        [
            ("id".to_string(), spg_storage::DataType::Uuid),
            ("prev_hash".to_string(), spg_storage::DataType::Text),
        ]
    );
}

/// UPDATE and DELETE bodies answer through the same resolver, and a
/// `WITH name(cols)` override renames positionally, as for a SELECT CTE.
#[test]
fn r1050_update_delete_ctes_and_column_overrides_describe() {
    let e = engine();
    assert_eq!(
        describe(
            &e,
            "WITH u AS (UPDATE ra SET h = 'z' RETURNING id, h) SELECT h, id FROM u",
        )
        .iter()
        .map(|(n, _)| n.as_str())
        .collect::<Vec<_>>(),
        ["h", "id"]
    );
    assert_eq!(
        describe(
            &e,
            "WITH d(gone) AS (DELETE FROM ra RETURNING k) SELECT gone FROM d",
        ),
        [("gone".to_string(), spg_storage::DataType::Text)]
    );
}

/// A data-modifying CTE with NO RETURNING has no columns to offer; a
/// SELECT reading it stays NoData rather than guessing.
#[test]
fn r1050_a_returning_less_cte_stays_nodata() {
    let e = engine();
    assert!(
        describe(
            &e,
            "WITH u AS (INSERT INTO ra (id, k, h) VALUES (gen_random_uuid(), 'x', 'y')) \
             SELECT 1 AS one FROM u",
        )
        .is_empty()
    );
}
