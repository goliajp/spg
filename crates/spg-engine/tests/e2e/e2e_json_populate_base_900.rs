//! 9.0.0 — `json_populate_record` ignored its base and refused a
//! composite one.
//!
//! The first argument is a BASE record: PostgreSQL keeps the fields the
//! JSON omits. Measured on 18.6 with `zz_jr AS (a int, b text, c int)`:
//!
//! ```text
//!   json_populate_record(ROW(9,'base',7)::jr, '{"a":1}')   1|base|7
//!   jsonb_populate_record(ROW(9,'base',7)::jr, '{"b":"new"}')  9|new|7
//!   json_populate_recordset(ROW(9,'base',7)::jr,
//!                           '[{"a":1},{"c":2}]')           1|base|7 / 9|base|2
//!   json_populate_record(NULL::jr, '{"a":1}')  (select list) (1,,)
//! ```
//!
//! SPG took the base only for its TYPE and filled every absent key with
//! NULL, so the documented use — patch a record with a JSON fragment —
//! lost every field the fragment left out. A non-NULL base did not even
//! get that far: the table-function context built its evaluation
//! context without the catalog, so `ROW(…)::jr` answered `type "jr"
//! does not exist` while the identical cast answered on its own. And
//! the select-list spelling had no arm at all.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    rows.into_iter()
        .map(|r| {
            r.values
                .iter()
                .map(spg_engine::eval::value_to_text)
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TYPE jr AS (a int, b text, c int)")
        .expect("create type");
    e
}

#[test]
fn the_base_supplies_what_the_json_omits() {
    let mut e = seeded();
    assert_eq!(
        rows(
            &mut e,
            "SELECT * FROM json_populate_record(ROW(9,'base',7)::jr, '{\"a\":1}')"
        ),
        vec!["1|base|7".to_string()]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT * FROM jsonb_populate_record(ROW(9,'base',7)::jr, '{\"b\":\"new\"}')"
        ),
        vec!["9|new|7".to_string()]
    );
    // Every element of a recordset patches the same base.
    assert_eq!(
        rows(
            &mut e,
            "SELECT * FROM json_populate_recordset(ROW(9,'base',7)::jr, '[{\"a\":1},{\"c\":2}]')"
        ),
        vec!["1|base|7".to_string(), "9|base|2".to_string()]
    );
}

#[test]
fn a_null_base_still_fills_with_nulls() {
    let mut e = seeded();
    assert_eq!(
        rows(
            &mut e,
            "SELECT * FROM json_populate_record(NULL::jr, '{\"a\":1}')"
        ),
        // `value_to_text` spells a NULL in process; the wire sends the
        // empty field PG's `1||` shows.
        vec!["1|NULL|NULL".to_string()]
    );
}

#[test]
fn the_select_list_spelling_answers_the_composite() {
    let mut e = seeded();
    assert_eq!(
        rows(&mut e, "SELECT json_populate_record(NULL::jr, '{\"a\":1}')"),
        vec!["(1,,)".to_string()]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT json_populate_record(ROW(9,'base',7)::jr, '{\"a\":1}')"
        ),
        vec!["(1,base,7)".to_string()]
    );
}
