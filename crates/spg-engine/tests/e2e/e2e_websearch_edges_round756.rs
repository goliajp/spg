//! Round 756 (F31-B7) — websearch_to_tsquery's operator words at the
//! edges, every answer PG18-measured (18/18 byte-identical over the
//! wire in the round-756 differential):
//!
//! - the word `or` is an OR operator only when it has a left operand
//!   and is not the last token; at operand position and at end of
//!   input it is a plain term;
//! - a `-` attaches ACROSS whitespace to the next word or phrase and
//!   STACKS (`--apple` → `!!'apple'`); the old tokenizer negated only
//!   a directly-attached word and dropped detached dashes.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, input: &str) -> String {
    let sql = format!("SELECT websearch_to_tsquery('simple', $q${input}$q$)");
    match e.execute(&sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn round756_bare_or_answers_as_pg() {
    let mut e = Engine::new();
    for (input, want) in [
        ("or", "'or'"),
        ("or apple", "'or' & 'apple'"),
        ("apple or", "'apple' & 'or'"),
        ("apple or or banana", "'apple' | 'or' & 'banana'"),
        ("or or", "'or' & 'or'"),
        ("apple or -", "'apple'"),
        ("or or and -", "'or' | 'and'"),
        ("a or b or c", "'a' | 'b' | 'c'"),
        ("\"or\" or apple", "'or' | 'apple'"),
    ] {
        assert_eq!(one(&mut e, input), want, "{input:?}");
    }
}

#[test]
fn round756_detached_and_stacked_dashes_answer_as_pg() {
    let mut e = Engine::new();
    for (input, want) in [
        ("- apple", "!'apple'"),
        ("apple - banana", "'apple' & !'banana'"),
        ("-or", "!'or'"),
        ("- or", "!'or'"),
        ("apple -banana", "'apple' & !'banana'"),
        ("-\"apple banana\"", "!( 'apple' <-> 'banana' )"),
        ("- \"a b\"", "!( 'a' <-> 'b' )"),
        ("--apple", "!!'apple'"),
        ("- - apple", "!!'apple'"),
    ] {
        assert_eq!(one(&mut e, input), want, "{input:?}");
    }
}
