//! v7.39 (round 516) — the catalog spellings of functions, and a comparator
//! that stopped answering Equal to everything it did not know.
//!
//! The `pg_proc` sweep's "user-facing" list was still over-reporting, for
//! the third distinct reason: it calls everything with NULLs, and that
//! decides "does this resolve", not "should this be implemented".
//! `setweight` and `json_to_record` were on the list and both already work
//! — what the sweep had actually probed was a 3-argument `setweight`
//! overload. So the candidates were re-asked with REAL arguments
//! (`scripts/user-facing-fn-diff.py`), and 3 of 36 agreed with PG18. This
//! round took it to 22.
//!
//! The comparator fix is the part worth reading. `network_larger` answered
//! the SMALLER address, because `value_cmp_for_min_max` ends in
//! `_ => Equal` and inet has its `network_cmp` in the OPERATOR path only.
//! `_ => Equal` is not a neutral default in a min/max comparator: it keeps
//! whichever value arrived first. It is the same fallback that made round
//! 511's `max(ctid)` answer `(0,1)`. It now delegates to the operator
//! comparison instead, so a type teaches the engine its order once.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    Engine::new()
}

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .unwrap_or_default(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// The per-type larger / smaller catalog functions. They were already this
/// engine's `greatest` / `least` arm; only the names were missing.
#[test]
fn round516_per_type_larger_and_smaller() {
    let mut e = engine();
    assert_eq!(text(&mut e, "SELECT oidlarger(1::oid, 2::oid)"), "2");
    assert_eq!(text(&mut e, "SELECT oidsmaller(1::oid, 2::oid)"), "1");
    assert_eq!(
        text(&mut e, "SELECT bpchar_larger('a'::bpchar, 'b'::bpchar)::text"),
        "b"
    );
    assert_eq!(
        text(&mut e, "SELECT tidlarger('(0,1)'::tid, '(0,2)'::tid)::text"),
        "(0,2)"
    );
}

/// The one that caught the comparator. inet orders by `network_cmp`, which
/// lived in the operator path only — so min/max kept the first argument.
#[test]
fn round516_network_larger_orders_by_the_network_comparison() {
    let mut e = engine();
    assert_eq!(
        text(
            &mut e,
            "SELECT network_larger('10.0.0.1'::inet, '10.0.0.2'::inet)::text"
        ),
        "10.0.0.2/32"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT network_smaller('10.0.0.1'::inet, '10.0.0.2'::inet)::text"
        ),
        "10.0.0.1/32"
    );
    // The delegation is general, not an inet special case: GREATEST over
    // the same pair agrees, and so does a type that already had an arm.
    assert_eq!(
        text(
            &mut e,
            "SELECT greatest('10.0.0.1'::inet, '10.0.0.2'::inet)::text"
        ),
        "10.0.0.2/32"
    );
}

/// The catalog names for functions SPG already had under their user-facing
/// spelling. PG exposes both, and a generated view can call either.
#[test]
fn round516_numeric_catalog_spellings() {
    let mut e = engine();
    assert_eq!(text(&mut e, "SELECT numeric_sqrt(4.0)::text"), "2.000000000000000");
    assert_eq!(text(&mut e, "SELECT numeric_div_trunc(7.0, 2.0)::text"), "3");
    assert_eq!(text(&mut e, "SELECT numeric_log(10.0, 100.0)::text"), "2.0000000000000000");
    assert_eq!(text(&mut e, "SELECT round(numeric_exp(1.0), 4)::text"), "2.7183");
    assert_eq!(text(&mut e, "SELECT numeric_ln(2.0)::text"), "0.6931471805599453");
    assert_eq!(text(&mut e, "SELECT textlen('abc')"), "3");
}

/// `count`'s transition functions, and the unique-name builder.
#[test]
fn round516_int8inc_int8dec_and_nameconcatoid() {
    let mut e = engine();
    assert_eq!(text(&mut e, "SELECT int8inc(1::bigint), int8dec(1::bigint)"), "2|0");
    assert_eq!(text(&mut e, "SELECT int8inc(NULL::bigint)"), "NULL");
    assert_eq!(text(&mut e, "SELECT nameconcatoid('abc', 42)"), "abc_42");
}

/// The function spellings of the geometric predicates, and the `to_reg*`
/// member that answers NULL rather than raising.
#[test]
fn round516_geometric_functions_and_to_regcollation() {
    let mut e = engine();
    assert_eq!(
        text(
            &mut e,
            "SELECT isparallel(lseg '((0,0),(1,1))', lseg '((0,1),(1,2))'), \
             isperp(lseg '((0,0),(1,1))', lseg '((0,1),(1,0))')"
        ),
        "true|true"
    );
    assert_eq!(text(&mut e, "SELECT to_regcollation('\"C\"')::text"), "\"C\"");
    // The `to_reg*` family answers NULL on a miss where the cast raises.
    assert_eq!(text(&mut e, "SELECT to_regcollation('nosuch')"), "NULL");
    assert!(e.execute("SELECT 'nosuch'::regcollation").is_err());
}

/// Every code point assigned?
#[test]
fn round516_unicode_assigned() {
    let mut e = engine();
    assert_eq!(text(&mut e, "SELECT unicode_assigned('abc')"), "true");
    assert_eq!(text(&mut e, "SELECT unicode_assigned(NULL::text)"), "NULL");
}
