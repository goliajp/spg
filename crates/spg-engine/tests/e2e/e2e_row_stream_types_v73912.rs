//! v7.39.12 — the row stream declares the same type Describe does.
//!
//! Reported by sentori against 7.39.11, and found by an alignment:
//! psql aligns a column by the type the ROW STREAM's `RowDescription`
//! carries, while `\gdesc` asks the extended protocol's Describe. They
//! disagreed for three kinds of expression, and psql left-aligned
//! columns PostgreSQL right-aligns.
//!
//! ```text
//!   SELECT count(*) AS plaincnt, count(*) OVER () AS wincnt, …
//!   PG 18   ········1·|······2·|…
//!   SPG     ········1·|·2······|…
//!                       ^^^^^^ only the window aggregate
//! ```
//!
//! Every expectation below is PostgreSQL 18.6's own `pg_typeof` for the
//! same expression, measured before anything was changed:
//!
//! ```text
//!   count(*) OVER ()                    bigint
//!   array_position(arr, 8::smallint)    integer     (over a plain smallint[])
//!   arr[1]                              smallint    and the column is named `arr`
//!   format_type over pg_index.indkey    int2vector
//! ```
//!
//! Three separate causes, one symptom. The window rewrite gave its
//! synthetic `__win_N` column `DataType::Text` under a comment saying
//! the type does not matter for projection eval — true of the eval, and
//! that type is what travels in the RowDescription. The array-position
//! family was missing from the function return map. And the
//! array-subscript arm carried its own three-entry element map where
//! the workspace already had a full one, so a `smallint[]` subscript
//! described itself as `smallint[]`.

use spg_engine::{Engine, QueryResult};

fn described(e: &mut Engine, sql: &str) -> (String, spg_storage::DataType) {
    let QueryResult::Rows { columns, .. } =
        e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows")
    };
    (columns[0].name.clone(), columns[0].ty)
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE bx (n int, arr smallint[])")
        .unwrap();
    e.execute("INSERT INTO bx VALUES (3, ARRAY[7,8]::smallint[])")
        .unwrap();
    e
}

#[test]
fn a_window_aggregate_is_bigint_like_the_plain_one_beside_it() {
    let mut e = seeded();
    let (_, plain) = described(&mut e, "SELECT count(*) AS plaincnt FROM bx");
    let (_, win) = described(&mut e, "SELECT count(*) OVER () AS wincnt FROM bx");
    assert_eq!(plain, spg_storage::DataType::BigInt);
    assert_eq!(
        win, plain,
        "the two descriptions of one count disagreed, and psql aligned them differently"
    );
}

#[test]
fn a_window_sum_reports_what_it_sums() {
    let mut e = seeded();
    // PostgreSQL widens `sum(int)` to bigint.
    let (_, ty) = described(&mut e, "SELECT sum(n) OVER () FROM bx");
    assert_eq!(ty, spg_storage::DataType::BigInt);
}

#[test]
fn array_position_is_an_integer() {
    let mut e = seeded();
    let (name, ty) = described(&mut e, "SELECT array_position(arr, 8::smallint) FROM bx");
    assert_eq!(ty, spg_storage::DataType::Int);
    assert_eq!(name, "array_position");
}

#[test]
fn a_subscript_reports_the_element_type_and_is_named_after_its_operand() {
    let mut e = seeded();
    let (name, ty) = described(&mut e, "SELECT arr[1] FROM bx");
    assert_eq!(ty, spg_storage::DataType::SmallInt, "not the array's type");
    assert_eq!(
        name, "arr",
        "PostgreSQL names a subscript after its operand"
    );
}

#[test]
fn format_type_knows_the_catalog_vectors() {
    // v7.39.11 gave these columns their own types and `pg_type` rows,
    // and `format_type` — which `information_schema.columns.data_type`
    // and `\d` are built on — answered `???` for exactly the five
    // columns that version had just retyped.
    let mut e = Engine::new();
    let QueryResult::Rows { rows, .. } = e
        .execute(
            "SELECT format_type(atttypid, atttypmod) FROM pg_attribute \
             WHERE attrelid = 'pg_index'::regclass \
               AND attname IN ('indkey','indclass','indoption','indcollation') \
             ORDER BY attname",
        )
        .unwrap()
    else {
        panic!("expected Rows")
    };
    let got: Vec<String> = rows
        .iter()
        .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
        .collect();
    assert_eq!(
        got,
        ["oidvector", "oidvector", "int2vector", "int2vector"],
        "indclass, indcollation, indkey, indoption"
    );
}

#[test]
fn the_neighbours_that_were_already_right_still_are() {
    // The control: everything sentori measured as identical on both
    // builds must stay identical, or a fix to the three above has
    // moved something that was not broken.
    let mut e = seeded();
    assert_eq!(
        described(&mut e, "SELECT sum(n) FROM bx").1,
        spg_storage::DataType::BigInt
    );
    assert_eq!(
        described(&mut e, "SELECT n+1 AS arith FROM bx").1,
        spg_storage::DataType::Int
    );
    assert_eq!(
        described(&mut e, "SELECT abs(n) FROM bx").1,
        spg_storage::DataType::Int
    );
    assert_eq!(
        described(&mut e, "SELECT 7::bigint AS cast7 FROM bx").1,
        spg_storage::DataType::BigInt
    );
    assert_eq!(
        described(&mut e, "SELECT n AS col FROM bx").1,
        spg_storage::DataType::Int
    );
}
