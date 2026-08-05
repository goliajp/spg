//! Round 760 (F31-B1) — the generic-operator precedence rung. PG binds
//! every "other" operator (`||`, `|`, `&`, `#`) BETWEEN additive and
//! the comparisons; SPG had them tied WITH `+ -` since v1 under a
//! "matches PG conceptually" comment the round-753 audit measured
//! false. Every answer below is PG18-measured (round-760 differential,
//! 16/16 byte-identical over the wire).

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(spg_engine::eval::value_to_text)
            .collect::<Vec<_>>()
            .join("|"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn round760_generic_operators_bind_below_additive() {
    let mut e = Engine::new();
    for (sql, want) in [
        // The round-753 finding: additive folds first, then concat.
        ("SELECT 'a' || 1 + 1", "a2"),
        ("SELECT 1 + 2 || '3'", "33"),
        ("SELECT -1 + 2 || 'z'", "1z"),
        ("SELECT 2 * 3 || '!'", "6!"),
        // Bitwise family — the old comment's ledgered divergence closes.
        ("SELECT 5 # 3 + 1", "1"),
        ("SELECT 12 | 2 + 3", "13"),
        ("SELECT 12 & 4 + 3", "4"),
        ("SELECT 5 - 1 # 2", "6"),
        // Comparisons still bind looser than the generic rung.
        ("SELECT (13 & 5 = 5)", "true"),
        // Same-level chains left-fold, as PG does.
        ("SELECT 3 # 2 # 1", "0"),
        ("SELECT 15 & 9 | 2", "11"),
        ("SELECT 'a' || 'b' || 'c'", "abc"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}
