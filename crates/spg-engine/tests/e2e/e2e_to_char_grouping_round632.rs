//! v7.39 (round 632, F33) — a group separator goes where the picture puts
//! it, and a lone `V` prints nothing.
//!
//! This closes the formatting half of F33:
//!
//!     single letters   56 differing -> 0
//!     keywords         13 differing -> 3
//!
//! and the three that remain are not formatting at all — `TH`, `th` and
//! `SS` on their own are ERRORS in PG (`"." is not a number`, `cannot use
//! "S" twice`) and answers here, which belongs with the other shapes SPG
//! accepts and PG refuses.
//!
//! Grouping was every three digits. PG fills the digit slots from the right
//! and emits each separator in the picture as `,` when digits are still to
//! be placed to its left and as a blank when they are not, so the two agree
//! only when the separator happens to sit on a thousands boundary — which
//! is exactly why `9G999` matched and `9G9` did not:
//!
//!     to_char(12,'9G9')       PG  ` 1,2`     SPG  `  12`
//!     to_char(123,'9G99')     PG  ` 1,23`    SPG  `  123`
//!     to_char(1234.5,'9G9')   PG  ` #,#`     SPG  `  ##`
//!
//! The overflow field keeps its separators too, and a separator with no
//! slot to its LEFT is not between two groups — `to_char(x,'G9')` is `  #`,
//! not ` ,#`, which the first cut of the overflow case got wrong.
//!
//! Measured against PG18 with `lc_monetary` and `lc_numeric` both `C`.

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

/// The separator sits where the picture puts it.
#[test]
fn round632_group_separator_is_positional() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT to_char(12,'9G9'), to_char(1,'9G9'), to_char(123,'9G99')"),
        vec![" 1,2|   1| 1,23"]
    );
    assert_eq!(
        vals(&mut e, "SELECT to_char(1234,'9G999'), to_char(12,'99G99')"),
        vec![" 1,234|    12"],
        "the thousands-boundary cases, which agreed all along"
    );
    // A literal comma behaves the same way.
    assert_eq!(
        vals(&mut e, "SELECT to_char(1234,'9,999'), to_char(1234567,'9G999G999')"),
        vec![" 1,234| 1,234,567"]
    );
    // FM trims the blanks but keeps the separator between real digits.
    assert_eq!(
        vals(&mut e, "SELECT to_char(12,'FM9G9'), to_char(1,'FM9G9'), to_char(1234,'FM9G999')"),
        vec!["1,2|1|1,234"]
    );
}

/// …including in an overflowed field.
#[test]
fn round632_overflow_keeps_its_separators() {
    let mut e = Engine::new();
    assert_eq!(vals(&mut e, "SELECT to_char(1234.5,'9G9')"), vec![" #,#"]);
    assert_eq!(
        vals(&mut e, "SELECT to_char(1234.5,'G9')"),
        vec!["  #"],
        "a separator with no slot to its left is not between two groups"
    );
    assert_eq!(vals(&mut e, "SELECT to_char(1234.5,'9')"), vec![" #"]);
}

/// A lone `V` scales nothing and prints nothing.
#[test]
fn round632_lone_v_prints_nothing() {
    let mut e = Engine::new();
    assert_eq!(vals(&mut e, "SELECT to_char(1,'V')"), vec![""]);
    // The forms that do have slots are unchanged.
    assert_eq!(vals(&mut e, "SELECT to_char(1,'9V'), to_char(1,'V9')"), vec![" 1| #"]);
    assert_eq!(vals(&mut e, "SELECT to_char(1,'9V9')"), vec![" 10"]);
}
