//! v7.38 (read01, T14) — named-argument notation `func(argname => value)` for
//! the make_* family: names resolve to positional order (reorderable, mixable
//! with leading positional args), and make_interval's omitted fields default to
//! zero. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("rows"),
    }
}

#[test]
fn named_arguments() {
    let mut e = Engine::new();
    assert_eq!(
        text(
            &mut e,
            "SELECT (make_date(year => 2024, month => 6, day => 15))::text"
        ),
        "2024-06-15"
    );
    // Reordered names.
    assert_eq!(
        text(
            &mut e,
            "SELECT (make_date(day => 15, month => 6, year => 2024))::text"
        ),
        "2024-06-15"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT (make_time(hour => 8, min => 30, sec => 0))::text"
        ),
        "08:30:00"
    );
    // make_interval omitted fields default to 0.
    assert_eq!(
        text(&mut e, "SELECT (make_interval(days => 40))::text"),
        "40 days"
    );
    // Leading positional + trailing named.
    assert_eq!(
        text(
            &mut e,
            "SELECT (make_date(2024, month => 6, day => 15))::text"
        ),
        "2024-06-15"
    );
    // Plain positional is unaffected, and `=` still compares.
    assert_eq!(
        text(&mut e, "SELECT (make_date(2024, 6, 15))::text"),
        "2024-06-15"
    );
    assert_eq!(text(&mut e, "SELECT (1 = 1)::text"), "true");
    // An unknown argument name errors.
    assert!(
        e.execute("SELECT make_date(nope => 2024, month => 6, day => 15)")
            .is_err()
    );
}
