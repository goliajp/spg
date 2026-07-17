//! v7.39 (read01 utils/adt, enum.c + float.c) — enum introspection
//! family (enum_first / enum_last / enum_range over the catalog's
//! member order, honoring ALTER TYPE ... ADD VALUE BEFORE) and
//! exact-value degree trigonometry (sind(30) = 0.5 exactly, PG
//! float.c semantics). All outputs differential-locked against PG18.

use spg_engine::{Engine, QueryResult};

fn text_of(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

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

#[test]
fn enum_first_last_range() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')")
        .unwrap();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT enum_first(NULL::mood), enum_last(NULL::mood)"
        ),
        vec!["sad", "happy"]
    );
    assert_eq!(
        text_of(&mut e, "SELECT enum_range(NULL::mood)"),
        "{sad,ok,happy}"
    );
    // Bounded forms: a NULL bound is an open end (PG).
    assert_eq!(
        text_of(&mut e, "SELECT enum_range('ok'::mood, NULL)"),
        "{ok,happy}"
    );
    assert_eq!(
        text_of(&mut e, "SELECT enum_range(NULL, 'ok'::mood)"),
        "{sad,ok}"
    );
}

#[test]
fn enum_range_sees_add_value_before() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')")
        .unwrap();
    e.execute("ALTER TYPE mood ADD VALUE 'meh' BEFORE 'ok'")
        .unwrap();
    // Member order, not lexicographic order.
    assert_eq!(
        text_of(&mut e, "SELECT enum_range(NULL::mood)"),
        "{sad,meh,ok,happy}"
    );
    assert_eq!(text_of(&mut e, "SELECT enum_first(NULL::mood)"), "sad");
}

#[test]
fn enum_introspection_from_column_reference() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')")
        .unwrap();
    e.execute("CREATE TABLE p (id INT NOT NULL, m mood)")
        .unwrap();
    e.execute("INSERT INTO p VALUES (1, 'ok')").unwrap();
    assert_eq!(text_of(&mut e, "SELECT enum_last(m) FROM p"), "happy");
}

#[test]
fn degree_trig_exact_at_standard_angles() {
    let mut e = Engine::new();
    // PG float.c: exact values at the standard angles.
    assert_eq!(
        row_of(&mut e, "SELECT sind(30), cosd(60), tand(45), cotd(45)"),
        vec!["0.5", "0.5", "1", "1"]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT sind(90), cosd(0), sind(150), cosd(120), tand(135)"
        ),
        vec!["1", "1", "0.5", "-0.5", "-1"]
    );
    // Inverse family: exact at the half/unit anchors.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT asind(0.5), acosd(0.5), atand(1), asind(-0.5), acosd(-1)"
        ),
        vec!["30", "60", "45", "-30", "180"]
    );
    // Poles render as PG does (cotd(0) = Infinity, tand(90) = Infinity).
    assert_eq!(
        row_of(&mut e, "SELECT cotd(90), tand(90), cotd(0)"),
        vec!["0", "Infinity", "Infinity"]
    );
}

#[test]
fn inverse_trig_domain_error() {
    let mut e = Engine::new();
    let err = e.execute("SELECT asind(2)").unwrap_err();
    assert!(
        format!("{err}").contains("input is out of range"),
        "PG's 22003 message, got: {err}"
    );
    // NaN propagates, does not error (PG).
    assert_eq!(text_of(&mut e, "SELECT asind('NaN'::float8)"), "NaN");
}
