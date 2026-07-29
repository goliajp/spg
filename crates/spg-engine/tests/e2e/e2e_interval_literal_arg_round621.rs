//! v7.39 (round 621) — an interval function refused the literal everyone
//! writes, and told them so in Rust.
//!
//! `SELECT justify_interval('36 hours')` answered `justify_interval() needs
//! interval, got Some(Text)`. PG answers `1 day 12:00:00`. Two faults in one
//! line: the query is refused, and the refusal prints a `Debug` of an internal
//! enum — `Some(Text)` is not a type name in any dialect.
//!
//! An unadorned string literal carries PG's `unknown` type and takes whatever
//! the chosen overload declares, which is the same rule round 620 applied to
//! the boolean connectives. Writing `INTERVAL` in front of the literal is
//! precisely what that rule saves you from, so the bare spelling is the one
//! people use.
//!
//! Only a LITERAL is resolved. `justify_interval(t)` over a TEXT column stays
//! refused, because PG refuses it too — there is no such overload. Telling the
//! two apart needs the argument's AST, so the resolution sits beside the enum
//! witness for `greatest`/`least`, which is there for the same reason.
//!
//! The failure mode comes out right for free: a literal that will not parse
//! gives the coercion's own error, `invalid input syntax for type interval:
//! "not an interval"`, which is PG's wording exactly — because it is the same
//! coercion.
//!
//! Measured and NOT closed:
//!
//!   * `justify_interval(1)` errors on both, differently: PG says the function
//!     does not exist for `integer`, SPG that it needs an interval. It no
//!     longer leaks `Some(Int)`;
//!   * `date_trunc('day', '2020-01-02 03:04:05')` and `extract(hour FROM
//!     '03:04:05')` are refused by PG as AMBIGUOUS (`is not unique` — two
//!     overloads could take the unknowns) and by SPG as a type error that
//!     still prints `Some(Text)`. Both refuse; neither the wording nor the
//!     reason matches;
//!   * that `{:?}` on an `Option<DataType>` appears at 383 sites across the
//!     engine. Three are fixed here, being the ones this round measured.

use spg_engine::{Engine, QueryResult};

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).expect_err(sql))
}

/// The three functions, bare literal and explicit INTERVAL alike.
#[test]
fn round621_a_bare_literal_is_an_interval_here() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT justify_interval('36 hours')"),
        vec!["1 day 12:00:00"]
    );
    assert_eq!(
        vals(&mut e, "SELECT justify_interval(INTERVAL '36 hours')"),
        vec!["1 day 12:00:00"],
        "the spelling that always worked answers the same"
    );
    assert_eq!(
        vals(&mut e, "SELECT justify_days('35 days'), justify_hours('27 hours')"),
        vec!["1 mon 5 days|1 day 03:00:00"]
    );
    assert_eq!(
        vals(&mut e, "SELECT justify_interval('1 year 15 months 100 hours')"),
        vec!["2 years 3 mons 4 days 04:00:00"],
        "months and hours both carry"
    );
    assert_eq!(
        vals(&mut e, "SELECT justify_interval(NULL)"),
        vec!["NULL"],
        "a NULL is not a literal to resolve"
    );
}

/// What stays refused, and in whose words.
#[test]
fn round621_what_is_not_a_literal_stays_refused() {
    let mut e = Engine::new();
    assert!(
        err(&mut e, "SELECT justify_interval('not an interval')")
            .contains(r#"invalid input syntax for type interval: "not an interval""#),
        "the coercion's own error, which is PG's wording: {}",
        err(&mut e, "SELECT justify_interval('not an interval')")
    );
    let mut e2 = Engine::new();
    e2.execute("CREATE TABLE iv (t TEXT)").unwrap();
    e2.execute("INSERT INTO iv VALUES ('36 hours')").unwrap();
    let m = err(&mut e2, "SELECT justify_interval(t) FROM iv");
    assert!(
        m.contains("needs interval") && !m.contains("Some("),
        "a TEXT COLUMN is not an unknown literal — PG has no such overload \
         either — and the refusal names a type, not a Rust enum: {m}"
    );
    let m = err(&mut e, "SELECT justify_interval(1)");
    assert!(
        m.contains("got integer") && !m.contains("Some("),
        "recorded rather than faked: PG says the function does not exist for \
         integer, SPG that it needs an interval — but no longer in Rust: {m}"
    );
}

/// The neighbours that must not have moved.
#[test]
fn round621_the_interval_neighbours() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT age('2020-06-01'::date, '2020-01-01'::date)"),
        vec!["5 mons"]
    );
    assert_eq!(
        vals(&mut e, "SELECT '36 hours'::interval + '1 day'"),
        vec!["1 day 36:00:00"],
        "interval arithmetic over a bare literal was already right"
    );
    assert_eq!(
        vals(&mut e, "SELECT justify_hours(INTERVAL '27 hours')"),
        vec!["1 day 03:00:00"]
    );
}
