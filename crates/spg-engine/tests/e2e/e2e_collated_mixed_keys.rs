//! v7.38.19 — a collated ORDER BY over a column holding BOTH kinds of
//! value.
//!
//! Under a collation whose `[0-9a-z]` order is byte order — `en_US`
//! among them — a value drawn from that alphabet needs no sort key: the
//! text is already in the right order, and building ICU's key for it
//! cost 1329.5 ms on 400,000 rows where PostgreSQL 18 spent 11.0 on the
//! same query.
//!
//! A value with a space or a capital in it still needs the key. So a
//! real column carries both, and the comparator has to put them back on
//! the same footing rather than compare a raw string against an ICU
//! key — which would order by whichever bytes happened to be larger.
//!
//! Every expectation here is PostgreSQL 18.4's, taken from it.

use spg_engine::{Engine, QueryResult};

fn ordered(e: &mut Engine, sql: &str) -> Vec<String> {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                spg_storage::Value::Text(t) => t.to_string(),
                spg_storage::Value::Null => "<NULL>".into(),
                other => format!("{other:?}"),
            })
            .collect(),
        other => panic!("expected rows from {sql}, got {other:?}"),
    }
}

fn seeded(values: &[&str]) -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE DATABASE mixed LC_COLLATE 'en_US.utf8'")
        .unwrap();
    e.execute("CREATE TABLE m (id int, s text)").unwrap();
    for (i, v) in values.iter().enumerate() {
        e.execute(&format!("INSERT INTO m VALUES ({i}, '{v}')"))
            .unwrap();
    }
    e
}

/// The interleaving is the test. `apple` and `zebra` are keyless, `Bob`
/// and `Yak` are keyed, and PostgreSQL 18.4 orders them
/// `apple, Bob, Yak, zebra` — every boundary between the two kinds
/// crossed at least once. Bytes alone answer `Bob, Yak, apple, zebra`.
#[test]
fn a_mixed_column_orders_the_way_postgresql_does() {
    let mut e = seeded(&["zebra", "Bob", "apple", "Yak"]);
    assert_eq!(
        ordered(&mut e, "SELECT s FROM m ORDER BY s"),
        ["apple", "Bob", "Yak", "zebra"]
    );
    assert_eq!(
        ordered(&mut e, "SELECT s FROM m ORDER BY s DESC"),
        ["zebra", "Yak", "Bob", "apple"]
    );
    assert_eq!(
        ordered(&mut e, "SELECT s FROM m ORDER BY s LIMIT 2"),
        ["apple", "Bob"]
    );
}

/// A value carrying a space is keyed; one carrying only the alphabet is
/// not. PostgreSQL orders `a b` before `ab`, which bytes agree with
/// here — but `ab c` after `abc`, which they do not.
#[test]
fn a_space_is_outside_the_alphabet_and_still_orders_correctly() {
    let mut e = seeded(&["abc", "ab c", "abd", "a b"]);
    assert_eq!(
        ordered(&mut e, "SELECT s FROM m ORDER BY s"),
        ["a b", "ab c", "abc", "abd"]
    );
}

/// Digits, the other half of the alphabet, against values that leave it.
#[test]
fn digits_and_letters_interleave_with_keyed_values() {
    let mut e = seeded(&["9zz", "1a", "1A", "abc", "ABC", "0"]);
    assert_eq!(
        ordered(&mut e, "SELECT s FROM m ORDER BY s"),
        ["0", "1a", "1A", "9zz", "abc", "ABC"]
    );
}

/// Ties. Two values the collation calls equal still order
/// deterministically, by their bytes — and that has to hold across the
/// two kinds too, since one side's bytes live inside its key.
#[test]
fn a_collation_tie_still_orders_by_bytes() {
    let mut e = seeded(&["abc", "ABC", "aBc"]);
    assert_eq!(
        ordered(&mut e, "SELECT s FROM m ORDER BY s"),
        ["abc", "aBc", "ABC"],
        "PostgreSQL 18.4 gives exactly this: the collation ties all three \
         and the bytes decide"
    );
}

/// A column with NO keyless value, and one with no keyed value: the two
/// homogeneous cases the mixed comparator must not disturb.
#[test]
fn the_homogeneous_columns_are_unchanged() {
    let mut all_keyed = seeded(&["Zebra", "Bob", "Apple"]);
    assert_eq!(
        ordered(&mut all_keyed, "SELECT s FROM m ORDER BY s"),
        ["Apple", "Bob", "Zebra"]
    );
    let mut none_keyed = seeded(&["zebra", "bob", "apple"]);
    assert_eq!(
        ordered(&mut none_keyed, "SELECT s FROM m ORDER BY s"),
        ["apple", "bob", "zebra"]
    );
}
