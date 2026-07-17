//! v7.39 (read01 utils/adt, array_userfuncs.c anchors) — PG17 array
//! functions + error shapes, every expected value the live PG18
//! oracle's output.

use spg_engine::{Engine, QueryResult};

fn text_of(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn array_sort_reverse_match_pg() {
    let mut e = Engine::new();
    assert_eq!(
        text_of(&mut e, "SELECT array_sort(ARRAY[3,1,2])"),
        "{1,2,3}"
    );
    // NULLS last ascending, first descending (PG's sort convention).
    assert_eq!(
        text_of(&mut e, "SELECT array_sort(ARRAY[3,1,NULL,2])"),
        "{1,2,3,NULL}"
    );
    assert_eq!(
        text_of(&mut e, "SELECT array_sort(ARRAY[3,1,NULL,2], true)"),
        "{NULL,3,2,1}"
    );
    assert_eq!(
        text_of(&mut e, "SELECT array_sort(ARRAY['b','a'], false, true)"),
        "{a,b}"
    );
    assert_eq!(
        text_of(&mut e, "SELECT array_reverse(ARRAY[1,2,3])"),
        "{3,2,1}"
    );
    assert_eq!(text_of(&mut e, "SELECT array_reverse(ARRAY['x'])"), "{x}");
}

#[test]
fn array_sample_and_multidim_search_error_shapes() {
    let mut e = Engine::new();
    // Out-of-range sample size errors like PG (22023 text), no clamp.
    let err = e
        .execute("SELECT array_sample(ARRAY[1,2,3], 5)")
        .unwrap_err();
    assert!(
        format!("{err}").contains("sample size must be between 0 and 3"),
        "got {err}"
    );
    // Multidimensional search refusal speaks PG's 0A000 text.
    let err = e
        .execute("SELECT array_position(ARRAY[[1,2]], 1)")
        .unwrap_err();
    assert!(
        format!("{err}")
            .contains("searching for elements in multidimensional arrays is not supported"),
        "got {err}"
    );
}
