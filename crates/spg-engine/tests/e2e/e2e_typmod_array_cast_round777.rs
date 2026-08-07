//! Round 777 (F31-E1) — a typmod'd array cast applies the modifier
//! per element, PG18-measured: ARRAY[1.5, 2.25]::numeric(3,1)[] is
//! {1.5,2.3} (rounded) and ::numeric(4,2)[] pads to {1.50,NULL,2.25}.
//! SPG answered 'type "numeric(3,1)_array" does not exist'.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn round777_typmod_array_cast_applies_per_element() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT ARRAY[1.5, 2.25]::numeric(3,1)[]"),
        "{1.5,2.3}"
    );
    assert_eq!(
        one(&mut e, "SELECT ARRAY[1.5, NULL, 2.25]::numeric(4,2)[]"),
        "{1.50,NULL,2.25}"
    );
    // The bare form is untouched.
    assert_eq!(one(&mut e, "SELECT ARRAY[1,2]::numeric[]"), "{1,2}");
}
