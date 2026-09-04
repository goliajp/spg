//! v7.39.13 — multi-argument `unnest` takes the same element types the
//! single-argument one does.
//!
//! Reported by sentori against 7.39.12, from a live 500 on a shipped
//! endpoint of theirs. `POST /v1/probes/sync` runs
//!
//! ```text
//!   INSERT INTO probes (…) SELECT i, $2, r, $3, $3
//!     FROM unnest($1::uuid[], $4::text[]) AS t(i, r)
//!     ON CONFLICT … DO UPDATE …
//! ```
//!
//! and every call returned `{"error":"internal"}` on every SPG build
//! they had tested. The boundary they measured, against PG 18.6:
//!
//! ```text
//!                                  PG 18.6   SPG 7.39.12
//!   unnest(text[], text[])              2         2
//!   unnest(int[], text[])               2         2
//!   unnest(bigint[], text[])            2         2
//!   unnest(uuid[])   — ONE argument     1         1
//!   unnest(uuid[], text[])              1       raises
//!   unnest(timestamptz[], text[])       1       raises
//! ```
//!
//! `unnest_zip_rows` carried its own three-entry element map — Text,
//! Int, BigInt — and refused everything else, while the single-argument
//! path used the workspace's full one. That is the third arm of a
//! sentence `array_element_at` already carries: "previously only
//! matched Text/Int/BigInt arrays and errored on every other element
//! type". It shares `array_elements` and `array_element_type` now, so
//! there is no list here to fall behind.
//!
//! The message was its own defect: `unnest() expects array arguments,
//! got uuid[]` names an array type as its reason for saying the
//! argument is not an array.

use spg_engine::{Engine, QueryResult};

fn count_of(e: &mut Engine, sql: &str) -> i64 {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows")
    };
    match &rows[0].values[0] {
        spg_storage::Value::BigInt(n) => *n,
        spg_storage::Value::Int(n) => i64::from(*n),
        other => panic!("{sql}: first value is {other:?}"),
    }
}

#[test]
fn multi_arg_unnest_zips_uuid_and_timestamptz_like_postgres() {
    let mut e = Engine::new();
    // The three that already worked stay working — without them a fix
    // that broke the common path would still pass the rows below.
    assert_eq!(
        count_of(
            &mut e,
            "SELECT count(*) FROM unnest(ARRAY['a','b']::text[], ARRAY['x','y']::text[]) AS t(a,b)"
        ),
        2
    );
    assert_eq!(
        count_of(
            &mut e,
            "SELECT count(*) FROM unnest(ARRAY[1,2], ARRAY['a','b']::text[]) AS t(a,b)"
        ),
        2
    );
    assert_eq!(
        count_of(
            &mut e,
            "SELECT count(*) FROM unnest(ARRAY['a']::text[], ARRAY['b']::text[], \
             ARRAY['c']::text[]) AS t(a,b,c)"
        ),
        1
    );

    // The two that raised. PG 18.6 answers 1 for each.
    assert_eq!(
        count_of(
            &mut e,
            "SELECT count(*) FROM unnest(\
             ARRAY['00000000-0000-0000-0000-000000000001']::uuid[], \
             ARRAY['a']::text[]) AS t(i,r)"
        ),
        1,
        "unnest(uuid[], text[]) — the shape a customer's endpoint runs"
    );
    assert_eq!(
        count_of(
            &mut e,
            "SELECT count(*) FROM unnest(\
             ARRAY['2026-01-01 00:00:00+00']::timestamptz[], ARRAY['a']::text[]) AS t(ts,r)"
        ),
        1,
        "unnest(timestamptz[], text[])"
    );
    // uuid on both sides, and uuid second — the map is symmetric or it
    // is not one.
    assert_eq!(
        count_of(
            &mut e,
            "SELECT count(*) FROM unnest(ARRAY['a']::text[], \
             ARRAY['00000000-0000-0000-0000-000000000002']::uuid[]) AS t(r,i)"
        ),
        1
    );
}

/// And the elements arrive as themselves, not as text. A zip that
/// produced the right ROW COUNT while flattening every element to text
/// would satisfy the counts above.
#[test]
fn the_zipped_columns_keep_their_element_types() {
    let mut e = Engine::new();
    let QueryResult::Rows { columns, .. } = e
        .execute(
            "SELECT i, r FROM unnest(\
             ARRAY['00000000-0000-0000-0000-000000000001']::uuid[], \
             ARRAY['a']::text[]) AS t(i, r)",
        )
        .expect("rows")
    else {
        panic!("expected rows")
    };
    assert_eq!(columns[0].ty, spg_storage::DataType::Uuid, "{columns:?}");
    assert_eq!(columns[1].ty, spg_storage::DataType::Text, "{columns:?}");
}

/// A genuine non-array still refuses, and the reason no longer names an
/// array type.
#[test]
fn a_non_array_argument_is_still_refused() {
    let mut e = Engine::new();
    let err = e
        .execute("SELECT * FROM unnest(1, ARRAY['a']::text[]) AS t(a, b)")
        .expect_err("an integer is not an array");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("expects array arguments"),
        "want the arity/type refusal, got: {msg}"
    );
    assert!(
        !msg.contains("[]"),
        "the reason must not name an array type as its reason for \
         saying the argument is not an array: {msg}"
    );
}
