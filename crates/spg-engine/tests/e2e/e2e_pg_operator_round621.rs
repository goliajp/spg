//! v7.39 (round 621) — `pg_operator` did not exist.
//!
//! `SELECT … FROM pg_operator` answered `relation "pg_operator" does not
//! exist` — the answer PG gives for a name it has never heard of, for one of
//! its own catalogs. psql's `\do` reads it, and so does anything asking what
//! `=` means between two given types.
//!
//! The rows are the operators SPG actually implements, over the types it
//! implements them for — 31 distinct names against PG's 74. Listing what is
//! there is the honest surface: claiming rows SPG cannot honour would be worse
//! than the missing relation, because a client would believe them. The row for
//! `=` between two `int4` matches PG's byte for byte.
//!
//! `oprcode` and the two selectivity estimators are 0 throughout — SPG's
//! operators are not catalogued functions, so there is nothing to name. That
//! is the choice `pg_type`'s seven I/O-function OIDs already made in round 543.
//!
//! Measured and NOT closed (filed as F25): no synthesised catalog's COLUMNS
//! appear in `pg_attribute` — `pg_type` and `pg_proc` answer 0 there as well,
//! where PG answers 38 and 36. It is uniform and predates this round, so
//! `pg_operator` joins them rather than standing out.

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

/// The relation, and the row a client looks up.
#[test]
fn round621_pg_operator_exists() {
    let mut e = Engine::new();
    assert_eq!(vals(&mut e, "SELECT count(*) > 0 FROM pg_operator"), vec!["true"]);
    assert_eq!(
        vals(
            &mut e,
            "SELECT oprname, oprleft, oprright, oprresult FROM pg_operator \
             WHERE oprname = '=' AND oprleft = 23 AND oprright = 23"
        ),
        vec!["=|23|23|16"],
        "int4 = int4 -> bool, which is PG's row for it exactly"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT oprname FROM pg_operator WHERE oprleft = 25 AND oprright = 25 \
             AND oprresult = 25"
        ),
        vec!["||"],
        "the one text operator that returns text"
    );
}

/// What the columns say about each operator.
#[test]
fn round621_the_columns_carry_their_meaning() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT oprcanmerge, oprcanhash FROM pg_operator \
             WHERE oprname = '=' AND oprleft = 23 AND oprright = 23"
        ),
        vec!["true|true"],
        "equality merges and hashes"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT oprcanmerge, oprcanhash FROM pg_operator \
             WHERE oprname = '<' AND oprleft = 23 AND oprright = 23"
        ),
        vec!["false|false"],
        "an ordering comparison does neither"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT oprkind, oprleft FROM pg_operator WHERE oprname = '-' AND oprkind = 'l' \
             AND oprright = 23"
        ),
        vec!["l|0"],
        "unary minus is 'l' with no left operand"
    );
    assert_eq!(
        vals(&mut e, "SELECT DISTINCT oprnamespace FROM pg_operator"),
        vec!["11"],
        "all of them live in pg_catalog"
    );
    assert_eq!(
        vals(&mut e, "SELECT count(DISTINCT oprcode) FROM pg_operator"),
        vec!["1"],
        "oprcode is 0 throughout — SPG's operators are not catalogued functions"
    );
}

/// The families that have to be there.
#[test]
fn round621_the_operator_families() {
    let mut e = Engine::new();
    for op in ["=", "<>", "<", "<=", ">", ">="] {
        assert!(
            !vals(
                &mut e,
                &format!("SELECT oprname FROM pg_operator WHERE oprname = '{op}' AND oprleft = 25")
            )
            .is_empty(),
            "comparison `{op}` over text"
        );
    }
    for op in ["+", "-", "*", "/"] {
        assert!(
            !vals(
                &mut e,
                &format!(
                    "SELECT oprname FROM pg_operator WHERE oprname = '{op}' AND oprleft = 1700"
                )
            )
            .is_empty(),
            "arithmetic `{op}` over numeric"
        );
    }
    for op in ["~~", "!~~", "~", "!~"] {
        assert!(
            !vals(
                &mut e,
                &format!("SELECT oprname FROM pg_operator WHERE oprname = '{op}'")
            )
            .is_empty(),
            "pattern `{op}`"
        );
    }
    assert!(
        !vals(&mut e, "SELECT oprname FROM pg_operator WHERE oprname = '->>' AND oprleft = 3802")
            .is_empty(),
        "jsonb ->> text"
    );
    assert!(
        !vals(&mut e, "SELECT oprname FROM pg_operator WHERE oprname = '&&' AND oprleft = 1007")
            .is_empty(),
        "array overlap"
    );
}
