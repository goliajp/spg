//! v7.39 (read01 utils/adt, round 40) — interval input forms
//! (decade/century/millennium units, @ prefix, trailing ago, bare
//! number + clock) and array_fill 2-D. Byte-locked vs PG18. cash/money
//! confirmed aligned.

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

#[test]
fn interval_input_forms() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT interval '1 decade', interval '1 century', interval '1 millennium', \
             interval '2 centuries'"
        ),
        vec!["10 years", "100 years", "1000 years", "200 years"]
    );
    // Bare number + clock is days; @ prefix decorative; trailing ago negates.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT interval '3 4:05:06', interval '@ 1 day 2 hours', interval '1 day ago'"
        ),
        vec!["3 days 04:05:06", "1 day 02:00:00", "-1 days"]
    );
}

#[test]
fn array_fill_two_dimensional() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT array_fill(7, ARRAY[3]), array_fill(0, ARRAY[2,2])"
        ),
        vec!["{7,7,7}", "{{0,0},{0,0}}"]
    );
}

#[test]
fn money_surface_aligned() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT '$1,234.56'::money, '($50.00)'::money, \
             '$10.00'::money + '$5.50'::money, '$100.00'::money / '$4.00'::money"
        ),
        vec!["$1,234.56", "-$50.00", "$15.50", "25"]
    );
}
