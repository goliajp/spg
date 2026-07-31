//! v7.39 (round 639) — pg_operator listed 132 of the 301 operator
//! combinations the engine evaluates.
//!
//! The catalog is generated: same-type comparisons over one type list,
//! same-type arithmetic over another. That leaves out everything cross-type
//! (`date + interval`, `int2 * int4`) and every type not in those lists —
//! time, json, jsonb, inet. Probed over PG's operators between SPG's base
//! types: **SPG evaluates 301 of the 308 PG evaluates**, and pg_operator
//! listed 132. The other 169 are here, each probed against the engine
//! before being listed, with PG's own `oprresult` for the result type.
//!
//! Measured as genuinely unsupported and therefore NOT listed:
//! `bytea ~~ bytea`, `bytea !~~ bytea`, `text @@ text`, and the four bpchar
//! pattern operators `~<~ ~<=~ ~>~ ~>=~`.
//!
//! The same round looked at `pg_type`, whose 82 rows against PG's 693 look
//! like a much bigger gap, and added nothing. `NULL::t` not erroring is not
//! evidence of support — the lesson round 633 paid for. Of the 52 types SPG
//! would cast to but does not list, `pg_typeof(NULL::t)` names a concrete
//! type for exactly two, and for both of those it answers `bigint`, not the
//! type asked for. Listing the other 50 would claim support the engine does
//! not have.

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

#[test]
fn round639_pg_operator_lists_what_the_engine_evaluates() {
    let mut e = Engine::new();
    assert_eq!(vals(&mut e, "SELECT count(*) FROM pg_operator"), vec!["330"]);
    // Cross-type arithmetic, which the same-type loops never produced.
    assert_eq!(
        vals(
            &mut e,
            "SELECT l.typname, o.oprname, r.typname, t.typname FROM pg_operator o \
             JOIN pg_type l ON l.oid = o.oprleft JOIN pg_type r ON r.oid = o.oprright \
             JOIN pg_type t ON t.oid = o.oprresult \
             WHERE l.typname = 'date' AND r.typname = 'interval' ORDER BY 2"
        ),
        vec!["date|+|interval|timestamp", "date|-|interval|timestamp"]
    );
    // A type the comparison loop never covered.
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM pg_operator o JOIN pg_type l ON l.oid = o.oprleft \
             WHERE l.typname = 'jsonb'"
        ),
        vec!["16"]
    );
}

/// Every binary row's three type ends resolve; the six that do not are the
/// unary operators, where PG also carries oprleft = 0.
#[test]
fn round639_only_the_unary_rows_have_no_left_type() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM pg_operator o \
             JOIN pg_type l ON l.oid = o.oprleft JOIN pg_type r ON r.oid = o.oprright \
             JOIN pg_type t ON t.oid = o.oprresult"
        ),
        vec!["324"]
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM pg_operator WHERE oprleft = 0"),
        vec!["6"],
        "the unary minus per numeric type — 330 - 324"
    );
    assert_eq!(
        vals(&mut e, "SELECT DISTINCT oprname, oprkind FROM pg_operator WHERE oprleft = 0"),
        vec!["-|l"]
    );
}

/// The operators the catalog deliberately does not claim.
#[test]
fn round639_unsupported_operators_stay_unlisted() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM pg_operator o JOIN pg_type l ON l.oid = o.oprleft \
             WHERE l.typname = 'bytea' AND o.oprname IN ('~~','!~~')"
        ),
        vec!["0"],
        "the engine cannot evaluate them, so the catalog does not offer them"
    );
    // …and the ones it can evaluate over bytea are listed.
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM pg_operator o JOIN pg_type l ON l.oid = o.oprleft \
             WHERE l.typname = 'bytea' AND o.oprname IN ('=','<','>','<>')"
        ),
        vec!["4"]
    );
}
