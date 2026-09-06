//! v7.40.11 — `pg_column_size` measured the length of a Rust `Debug`
//! string.
//!
//! Reported against 7.40.9: the function contradicted the catalog's own
//! `typlen` in the same build.
//!
//! ```text
//!                  typlen   pg_column_size   PG 18.6
//!   uuid              16          58            16
//!   timestamptz        8          31             8
//!   date               4          15             4
//!   numeric            —          50            10
//! ```
//!
//! 58 is `Uuid([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1])` plus
//! the four-byte varlena header the fallback adds — the fixed-width
//! types were not named in the match, so they reached an arm whose own
//! comment calls itself an approximation for composite and array types.
//!
//! `sum(pg_column_size(col))` is the ordinary way to size a column
//! before a migration, and this over-reported `uuid` by 3.6x. It also
//! put the internal representation into a user-facing number, which is
//! the same class as the `UuidArray([Some([0, …]))` that appeared in an
//! error message the same round.
//!
//! Every expectation below is measured on PostgreSQL 18.6, including
//! the five numeric sizes that establish its base-10000 grouping.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn size(eng: &mut Engine, expr: &str) -> i32 {
    let sql = format!("SELECT pg_column_size({expr})");
    match eng.execute(&sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
        QueryResult::Rows { rows, .. } => match rows[0].values[0] {
            Value::Int(n) => n,
            ref other => panic!("{sql}: {other:?}"),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn the_fixed_width_types_report_their_width() {
    let mut eng = Engine::new();
    // (expression, PG 18.6's answer)
    let cases: &[(&str, i32)] = &[
        ("1::smallint", 2),
        ("1::int4", 4),
        ("1::int8", 8),
        ("1::real", 4),
        ("1::float8", 8),
        ("true", 1),
        ("'00000000-0000-0000-0000-000000000001'::uuid", 16),
        ("'2026-01-01 00:00:00+00'::timestamptz", 8),
        ("'2026-01-01 00:00:00'::timestamp", 8),
        ("'2026-01-01'::date", 4),
        ("'12:00'::time", 8),
        ("'1 day'::interval", 16),
    ];
    for (expr, want) in cases {
        assert_eq!(size(&mut eng, expr), *want, "pg_column_size({expr})");
    }
}

/// PostgreSQL stores a numeric in base-10000 groups: a six-byte short
/// header plus two bytes per group, counted on each side of the point
/// separately. These five readings are what establishes that rule —
/// `1.5` is `1` and `5000`, two groups, so ten bytes.
#[test]
fn numeric_follows_postgres_grouping() {
    let mut eng = Engine::new();
    let cases: &[(&str, i32)] = &[
        ("0::numeric", 6),
        ("1::numeric", 8),
        ("1.5::numeric", 10),
        ("12345::numeric", 10),
        ("123456789012345678901234567890::numeric", 22),
    ];
    for (expr, want) in cases {
        assert_eq!(size(&mut eng, expr), *want, "pg_column_size({expr})");
    }
}

/// The variable-width types, which were already right and must stay so:
/// the varlena header plus the bytes.
#[test]
fn the_variable_width_types_are_unchanged() {
    let mut eng = Engine::new();
    assert_eq!(size(&mut eng, "'abc'::text"), 7);
    assert_eq!(size(&mut eng, "''::text"), 4);
    assert_eq!(size(&mut eng, "'\\x010203'::bytea"), 7);
}

/// The function and the catalog must agree about one fact. This is the
/// contradiction as it was reported, asserted directly.
#[test]
fn the_function_agrees_with_typlen() {
    let mut eng = Engine::new();
    for (ty, expr) in [
        ("uuid", "'00000000-0000-0000-0000-000000000001'::uuid"),
        ("timestamptz", "'2026-01-01 00:00:00+00'::timestamptz"),
        ("date", "'2026-01-01'::date"),
        ("int8", "1::int8"),
    ] {
        let sql = format!("SELECT typlen FROM pg_type WHERE typname = '{ty}'");
        let typlen = match eng.execute(&sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
            QueryResult::Rows { rows, .. } => match rows.first().map(|r| r.values[0].clone()) {
                Some(Value::SmallInt(n)) => i32::from(n),
                Some(Value::Int(n)) => n,
                other => panic!("{sql}: {other:?}"),
            },
            other => panic!("{sql}: {other:?}"),
        };
        assert_eq!(
            size(&mut eng, expr),
            typlen,
            "{ty}: pg_column_size and typlen describe the same width"
        );
    }
}
