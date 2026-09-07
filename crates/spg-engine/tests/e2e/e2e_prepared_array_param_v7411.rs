//! v7.40.11 — a `PREPARE`-declared array parameter arrived as the Rust
//! `Debug` form of its own value.
//!
//! Reported against 7.40.9 (§3.8) for `uuid[]` and `timestamptz[]`:
//!
//! ```text
//!   PREPARE q (uuid[]) AS SELECT * FROM unnest($1);
//!   EXECUTE q (ARRAY['…']::uuid[]);
//!     ERROR:  unnest() expects an array argument, got text
//! ```
//!
//! and, where the coercion raised instead of the function, the internal
//! spelling came back in a user-facing error — the reporter quoted it:
//!
//! ```text
//!   ERROR:  malformed array literal: "UuidArray([Some([0, 0, …, 1])])"
//! ```
//!
//! The wire path (`\bind`) was fixed in 7.39.12; the SQL-level
//! `PREPARE`/`EXECUTE` path was not, and the reporter's own repro
//! carries both separately for exactly that reason.
//!
//! The placeholder substituted through `clock::value_to_literal`, which
//! returns a `Literal` — a type that can express an array only as
//! `Literal::{Text,Int,BigInt}Array`, with a last arm that renders
//! anything else as `format!("{v:?}")`. So `bigint[]` and `text[]` had
//! arms and worked, and every other element type became Debug text.
//!
//! It is wider than the two that were filed. Measured on 7.40.10, all
//! six through `PREPARE (T) … EXECUTE`:
//!
//! ```text
//!   bigint[]       rows
//!   text[]         rows
//!   uuid[]         ERROR  unnest() expects an array argument, got text
//!   timestamptz[]  ERROR  same
//!   numeric[]      ERROR  same
//!   inet[]         ERROR  same
//! ```
//!
//! The placeholder now goes through the EXPRESSION funnel, which
//! rebuilds an array as the `ARRAY[…]` it came from using the per-type
//! element menu the rest of the engine already uses.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(eng: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    match eng.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// The two element types that were reported, and the four more the same
/// defect covered.
#[test]
fn every_declared_array_type_reaches_unnest_as_an_array() {
    let mut eng = Engine::new();
    // (declared type, the argument, how many rows unnest must yield)
    let cases: &[(&str, &str, usize)] = &[
        (
            "uuid[]",
            "ARRAY['00000000-0000-0000-0000-000000000001',\
             '00000000-0000-0000-0000-000000000002']::uuid[]",
            2,
        ),
        (
            "timestamptz[]",
            "ARRAY['2026-01-01 00:00:00+00','2026-06-01 00:00:00+00']::timestamptz[]",
            2,
        ),
        ("numeric[]", "ARRAY[1.50,2.25]::numeric[]", 2),
        ("inet[]", "ARRAY['10.0.0.1']::inet[]", 1),
        // The two that already worked, so a fix that reaches too far
        // cannot quietly change them.
        ("bigint[]", "ARRAY[1,2]::bigint[]", 2),
        ("text[]", "ARRAY['a','b']::text[]", 2),
    ];
    for (i, (ty, arg, n)) in cases.iter().enumerate() {
        let name = format!("pa{i}");
        eng.execute(&format!(
            "PREPARE {name} ({ty}) AS SELECT * FROM unnest($1)"
        ))
        .unwrap_or_else(|e| panic!("PREPARE ({ty}): {e}"));
        let got = rows(&mut eng, &format!("EXECUTE {name} ({arg})"));
        assert_eq!(got.len(), *n, "{ty}: {got:?}");
    }
}

/// The VALUES, not just the row count — a Debug-form literal that
/// happened to parse would pass a count assertion.
#[test]
fn the_elements_are_the_values_that_went_in() {
    let mut eng = Engine::new();
    eng.execute("PREPARE pu (uuid[]) AS SELECT * FROM unnest($1)")
        .expect("prepare");
    let got = rows(
        &mut eng,
        "EXECUTE pu (ARRAY['00000000-0000-0000-0000-000000000001']::uuid[])",
    );
    assert_eq!(
        got,
        vec![vec![Value::Uuid([
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1
        ])]]
    );

    eng.execute("PREPARE pt (timestamptz[]) AS SELECT * FROM unnest($1)")
        .expect("prepare");
    let got = rows(
        &mut eng,
        "EXECUTE pt (ARRAY['2026-01-01 00:00:00+00']::timestamptz[])",
    );
    assert_eq!(got, vec![vec![Value::Timestamp(1_767_225_600_000_000)]]);
}

/// No user-facing message may carry a Rust `Debug` rendering. That is
/// what the reporter saw, and it is the same class as the `uuid` that
/// `pg_column_size` measured the Debug length of.
#[test]
fn no_error_leaks_the_internal_spelling() {
    let mut eng = Engine::new();
    eng.execute("PREPARE pz (uuid[]) AS SELECT $1")
        .expect("prepare");
    // Whatever this does, it must not print `UuidArray([Some([…])])`.
    let msg =
        match eng.execute("EXECUTE pz (ARRAY['00000000-0000-0000-0000-000000000001']::uuid[])") {
            Ok(_) => String::new(),
            Err(e) => format!("{e}"),
        };
    assert!(
        !msg.contains("UuidArray(") && !msg.contains("Some(["),
        "the internal value reached the user: {msg}"
    );
}

/// The other places a placeholder lands, because the substitution is
/// one function and the array had to survive all of them.
#[test]
fn a_declared_array_parameter_works_outside_unnest_too() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE pk (id UUID PRIMARY KEY, n INT)")
        .expect("ddl");
    eng.execute(
        "INSERT INTO pk VALUES ('00000000-0000-0000-0000-000000000001', 1), \
         ('00000000-0000-0000-0000-000000000002', 2)",
    )
    .expect("insert");
    eng.execute("PREPARE pany (uuid[]) AS SELECT n FROM pk WHERE id = ANY($1) ORDER BY n")
        .expect("prepare");
    let got = rows(
        &mut eng,
        "EXECUTE pany (ARRAY['00000000-0000-0000-0000-000000000002']::uuid[])",
    );
    assert_eq!(got, vec![vec![Value::Int(2)]], "= ANY over a uuid[]");
}
