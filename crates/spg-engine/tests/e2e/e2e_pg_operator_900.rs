//! 9.0.0 (N22) — `pg_operator` carried oids nobody could resolve.
//!
//! The table was generated: 330 rows, oids counting up from 70,000,
//! `oprcom` and `oprnegate` pointing at those invented oids and `oprcode`
//! the literal `-`. Every row was internally consistent and externally
//! useless — a client that reads `oprnegate` to turn `NOT (a = b)` into
//! `a <> b`, or `oprcode` to name the function behind an operator, got an
//! oid PostgreSQL has never issued.
//!
//! It carries PG 18.6's own rows now, for the 690 operators a NON-NULL
//! probe showed SPG evaluates. `oprcom` and `oprnegate` land on rows of
//! this same table, and `oprcode` names PG's function.

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

/// The commutator and the negator are rows of this table, not dangling oids.
#[test]
fn n22_commutator_and_negator_resolve() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT o.oprname, c.oprname, n.oprname FROM pg_operator o \
             JOIN pg_operator c ON c.oid = o.oprcom \
             JOIN pg_operator n ON n.oid = o.oprnegate \
             WHERE o.oid = 96"
        ),
        vec!["=|=|<>"],
        "int4 = int4 commutes with itself and negates to <>"
    );
    // The generated table pointed these at oids from 70,000 up, so the
    // join above returned nothing at all for every row.
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM pg_operator o JOIN pg_operator c ON c.oid = o.oprcom"
        ),
        vec!["503"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM pg_operator o JOIN pg_operator n ON n.oid = o.oprnegate"
        ),
        vec!["360"]
    );
}

/// `oprcode` names PostgreSQL's function, and the oid is PostgreSQL's oid.
#[test]
fn n22_oprcode_names_the_function() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT oid, oprcode, oprrest, oprjoin FROM pg_operator WHERE oprname = '=' \
             AND oprleft = 23 AND oprright = 23"
        ),
        vec!["96|int4eq|eqsel|eqjoinsel"],
        "PG's row for int4 = int4, oid and all"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM pg_operator WHERE oprcode::text = '-' OR oid >= 70000"
        ),
        vec!["0"],
        "nothing invented is left"
    );
}

/// The polymorphic operand types have no literal of their own. Probing with
/// a concrete one — `int4[]` for `anyarray`, `int4range` for `anyrange` —
/// is what put these 82 rows in the table.
#[test]
fn n22_polymorphic_operators_are_listed() {
    let mut e = Engine::new();
    // The count is PG's own: `&&` over `anyrange` is declared twice,
    // once against `anyrange` and once against `anymultirange`.
    for (name, left, n, what) in [
        ("&&", 2277, "1", "array overlap"),
        ("@>", 2277, "1", "array contains"),
        ("&&", 3831, "2", "range overlap"),
        ("-|-", 3831, "2", "range adjacent"),
        ("&&", 4537, "2", "multirange overlap"),
        ("=", 2249, "1", "record equality"),
        ("=", 3500, "1", "enum equality"),
    ] {
        assert_eq!(
            vals(
                &mut e,
                &format!(
                    "SELECT count(*) FROM pg_operator WHERE oprname = '{name}' \
                     AND oprleft = {left}"
                )
            ),
            vec![n],
            "{what}"
        );
    }
    // `@?` over jsonpath is listed; `@@` over jsonpath is not, because
    // SPG cannot parse a jsonpath predicate — the probe failed there and
    // PG answered it, so the difference is SPG's.
    assert_eq!(
        vals(
            &mut e,
            "SELECT oprname FROM pg_operator WHERE oprleft = 3802 AND oprright = 4072 \
             ORDER BY 1"
        ),
        vec!["@?"]
    );
}
