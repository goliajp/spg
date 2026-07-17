//! v7.39 (read01 utils/adt, round 27) — regproc.c: regtype canonical
//! rendering + existence, the regproc/regprocedure/regoper/regconfig/
//! regdictionary input casts, and the to_reg* family's name rendering.
//! ri_triggers.c verified zero-delta (CASCADE / SET NULL / SET DEFAULT /
//! UPDATE CASCADE). Byte-locked vs PG18.

use spg_engine::{Engine, QueryResult};

fn row_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(spg_engine::eval::value_to_text)
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err_of(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

#[test]
fn regtype_canonical_and_existence() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT 'int4'::regtype, 'pg_catalog.int4'::regtype, 'integer[]'::regtype, \
             '_int4'::regtype, 23::regtype"
        ),
        vec!["integer", "integer", "integer[]", "integer[]", "integer"]
    );
    assert!(
        err_of(&mut e, "SELECT 'nosuchtype'::regtype")
            .contains("type \"nosuchtype\" does not exist")
    );
}

#[test]
fn regproc_family_casts() {
    let mut e = Engine::new();
    // lower is ambiguous (text + anyrange overloads, as in PG).
    assert!(
        err_of(&mut e, "SELECT 'lower'::regproc")
            .contains("more than one function named \"lower\"")
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT 'lower(text)'::regprocedure, 'sum(int4)'::regprocedure, \
             'now()'::regprocedure::text"
        ),
        vec!["lower(text)", "sum(integer)", "now()"]
    );
    assert!(err_of(&mut e, "SELECT '+'::regoper").contains("more than one operator named +"));
    assert_eq!(
        row_of(
            &mut e,
            "SELECT 'english'::regconfig, 'simple'::regdictionary"
        ),
        vec!["english", "simple"]
    );
    assert!(
        err_of(&mut e, "SELECT 'klingon'::regconfig")
            .contains("text search configuration \"klingon\" does not exist")
    );
}

#[test]
fn to_reg_family_renders_names() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE regp_t (id int)").unwrap();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT to_regtype('int4'), to_regtype('integer'), to_regclass('regp_t'), \
             to_regclass('pg_class'), to_regnamespace('public')"
        ),
        vec!["integer", "integer", "regp_t", "pg_class", "public"]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT to_regtype('nosuchtype'), to_regclass('nosuch'), to_regproc('lower'), \
             to_regproc('now')"
        ),
        vec!["NULL", "NULL", "NULL", "now"]
    );
}
