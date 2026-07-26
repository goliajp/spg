//! v7.39 (round 513) — the rest of the `reg*` family.
//!
//! The function sweep's type inventory listed nine of them. Six already
//! worked — `regclass`, `regtype`, `regconfig`, `regdictionary`, `regproc`,
//! `regprocedure`, the last two down to PG's own "more than one function
//! named abs" — and three did not: `regnamespace`, `regcollation`,
//! `regrole`.
//!
//! Each resolves against something the type table cannot see: schemas live
//! on the catalog, roles on the engine. So they are settled in the cast arm
//! that HAS those, the same place round 509 put the table row types.
//!
//! `regcollation` has a wrinkle worth keeping. PG lowercases an UNQUOTED
//! identifier before it looks, so `'C'::regcollation` is "collation \"c\"
//! for encoding \"UTF8\" does not exist" while `'"C"'::regcollation`
//! resolves — which is why the quoted form is the one anybody writes.
//!
//! `regoperator` and `regoper` stay as they were: resolving them needs an
//! operator catalog SPG does not have, and they already answer PG's
//! ambiguity error for a core symbol.
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

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).expect_err(sql))
}

#[test]
fn round513_regnamespace_resolves_a_schema() {
    let mut e = engine();
    assert_eq!(
        text(
            &mut e,
            "SELECT 'pg_catalog'::regnamespace::text, 'public'::regnamespace::text, \
             'information_schema'::regnamespace::text"
        ),
        "pg_catalog|public|information_schema"
    );
    assert!(
        err(&mut e, "SELECT 'nosuchschema'::regnamespace")
            .contains("schema \"nosuchschema\" does not exist")
    );
    // A schema the session made resolves too.
    e.execute("CREATE SCHEMA app").unwrap();
    assert_eq!(text(&mut e, "SELECT 'app'::regnamespace::text"), "app");
}

#[test]
fn round513_regcollation_lowercases_an_unquoted_name() {
    let mut e = engine();
    assert_eq!(
        text(
            &mut e,
            "SELECT '\"C\"'::regcollation::text, '\"POSIX\"'::regcollation::text, \
             'ucs_basic'::regcollation::text, 'default'::regcollation::text"
        ),
        "\"C\"|\"POSIX\"|ucs_basic|\"default\""
    );
    // Unquoted `C` is folded to `c` BEFORE the lookup, so it does not
    // resolve — and the error says `c`, not `C`.
    let got = err(&mut e, "SELECT 'C'::regcollation");
    assert!(
        got.contains("collation \"c\" for encoding \"UTF8\" does not exist"),
        "got {got}"
    );
    assert!(
        err(&mut e, "SELECT 'nosuchcoll'::regcollation")
            .contains("collation \"nosuchcoll\" for encoding \"UTF8\" does not exist")
    );
}

#[test]
fn round513_regrole_resolves_roles_and_the_predefined_ones() {
    let mut e = engine();
    assert_eq!(text(&mut e, "SELECT 'admin'::regrole::text"), "admin");
    // PG's predefined roles exist whether or not anybody created them.
    assert_eq!(
        text(
            &mut e,
            "SELECT 'pg_read_all_data'::regrole::text, 'pg_monitor'::regrole::text"
        ),
        "pg_read_all_data|pg_monitor"
    );
    assert!(
        err(&mut e, "SELECT 'nosuchrole'::regrole").contains("role \"nosuchrole\" does not exist")
    );
}

/// All three are real type names, so a NULL cast resolves rather than
/// tripping round 509's unknown-target check.
#[test]
fn round513_null_casts_to_the_new_reg_types() {
    let mut e = engine();
    assert_eq!(
        text(
            &mut e,
            "SELECT NULL::regrole, NULL::regnamespace, NULL::regcollation"
        ),
        "NULL|NULL|NULL"
    );
}

/// The six that already worked keep working — this is the regression half.
#[test]
fn round513_the_existing_reg_types_are_unchanged() {
    let mut e = engine();
    e.execute("CREATE TABLE t (a INT)").unwrap();
    assert_eq!(
        text(
            &mut e,
            "SELECT 't'::regclass::text, 'int4'::regtype::text, \
             'english'::regconfig::text, 'simple'::regdictionary::text, \
             'abs(int)'::regprocedure::text"
        ),
        "t|integer|english|simple|abs(integer)"
    );
}
