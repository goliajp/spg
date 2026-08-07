//! v7.39 (round 597) — the right-hand array of an ANY/ALL was rebuilt for
//! every row.
//!
//! The ledger's top entry after round 596 was a CTE self-join at 40.8x, and
//! it was not real: PG answers that query with `column reference "n" is
//! ambiguous`, so the 0.30 ms was an error being raised. SPG raises the same
//! error, just after 12 ms of work — two error paths being compared, not two
//! answers. The ledger now says so. The other three sweep entries were
//! checked the same way and do hold: PG really scans with the filter, really
//! runs the Recursive Union, really runs the ProjectSet.
//!
//! That left `= ANY (ARRAY[…])`, and its cost grew with the array:
//!
//!     id = ANY (ARRAY[1])                56.00 ->  1.84 ms   PG 5.64
//!     id = ANY (ARRAY[1..10])           268.09 ->  1.93      PG 8.33
//!     id = ANY (ARRAY[1..20])           493.83 ->  1.97      PG 7.79
//!     id <> ALL (ARRAY[1..5])           149.19 ->  3.31      PG 9.46
//!     s = ANY (ARRAY['row1','row2',…])  147.31 ->  1.95      PG 8.76
//!     id = ANY ('{1..10}'::INT[])       286.60 -> 44.53      PG 7.21
//!     id > ANY (ARRAY[1,2,3])           103.24 -> 17.21      PG 7.60
//!
//! The tell was the equivalent spelling: `id IN (1..10)` was already 2.3 ms
//! and beating PG, because an IN list becomes a membership set at compile
//! time. `ARRAY[…]` was an ordinary expression, evaluated per row — 500k
//! reconstructions of the same ten-element array, then a linear walk.
//!
//! Two steps, and the second is why the numbers stop tracking the array's
//! length. A constant right-hand array is now built once whatever the
//! operator, which is what the last two rows above take. And `x = ANY (…)`
//! is `x IN (…)`, `x <> ALL (…)` is `x NOT IN (…)` — down to the
//! three-valued treatment of a NULL element — so those take the same
//! membership set the IN list builds, and five of the seven shapes now beat
//! PG outright.
//!
//! What the pins are for. The equivalence has to hold where SQL is at its
//! least obvious: a NULL element makes a non-match NULL rather than false, a
//! NULL left-hand side makes everything NULL, and an EMPTY array is decided
//! by emptiness alone — false for ANY, true for ALL — before the left-hand
//! side is even looked at. All 20 shapes here were checked against live PG18
//! and matched.

use spg_engine::{Engine, QueryResult};

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE an (id INT, b BIGINT, n NUMERIC(10,2), s TEXT)")
        .unwrap();
    e.execute(
        "INSERT INTO an VALUES (1,1,1.00,'a'),(2,2,2.50,'b'),(3,NULL,NULL,NULL),\
         (4,4,4.00,'d'),(5,5,5.00,'e')",
    )
    .unwrap();
    e
}

/// The two shapes that become a membership set, and the three-valued cases
/// that decide whether they may.
#[test]
fn round597_any_all_three_valued_logic() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, id = ANY (ARRAY[1,3,5]) FROM an ORDER BY id"
        ),
        vec!["1|true", "2|false", "3|true", "4|false", "5|true"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, id = ANY (ARRAY[1,NULL,5]) FROM an ORDER BY id"
        ),
        vec!["1|true", "2|NULL", "3|NULL", "4|NULL", "5|true"],
        "a NULL element makes a non-match NULL, not false"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, id <> ALL (ARRAY[1,3]) FROM an ORDER BY id"
        ),
        vec!["1|false", "2|true", "3|false", "4|true", "5|true"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, id <> ALL (ARRAY[1,NULL]) FROM an ORDER BY id"
        ),
        vec!["1|false", "2|NULL", "3|NULL", "4|NULL", "5|NULL"],
        "NOT IN semantics: only a positive match escapes the NULL"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, b = ANY (ARRAY[1,2]), b <> ALL (ARRAY[1,2]) FROM an ORDER BY id"
        ),
        vec![
            "1|true|false",
            "2|true|false",
            "3|NULL|NULL",
            "4|false|true",
            "5|false|true",
        ],
        "a NULL left-hand side"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, id = ANY (ARRAY[]::INT[]), id <> ALL (ARRAY[]::INT[]) FROM an ORDER BY id"
        ),
        vec![
            "1|false|true",
            "2|false|true",
            "3|false|true",
            "4|false|true",
            "5|false|true",
        ],
        "an empty array is decided by emptiness alone"
    );
    // The equivalence itself, asked of the engine.
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM an WHERE (id = ANY (ARRAY[1,NULL,5])) \
             IS NOT DISTINCT FROM (id IN (1,NULL,5))"
        ),
        vec!["5"],
        "every row agrees with the IN spelling, NULLs and all"
    );
}

/// Element types that are not the column's own, and the spellings that do
/// not carry their elements in the tree.
#[test]
fn round597_element_types_and_spellings() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, s = ANY (ARRAY['a','d']) FROM an ORDER BY id"
        ),
        vec!["1|true", "2|false", "3|NULL", "4|true", "5|false"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, id = ANY (ARRAY[1.00, 4.00]), b = ANY (ARRAY[1,4]) FROM an ORDER BY id"
        ),
        vec![
            "1|true|true",
            "2|false|false",
            "3|false|NULL",
            "4|true|true",
            "5|false|false",
        ],
        "NUMERIC elements against an INT column, and INT against BIGINT"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM an WHERE id = ANY (ARRAY[2::BIGINT, 4::BIGINT]) ORDER BY id"
        ),
        vec!["2", "4"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, id = ANY ('{1,3}'::INT[]) FROM an ORDER BY id"
        ),
        vec!["1|true", "2|false", "3|true", "4|false", "5|false"],
        "the elements live in a string here, so this keeps the folded-array path"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, id = ANY (ARRAY[2,2,2]) FROM an ORDER BY id"
        ),
        vec!["1|false", "2|true", "3|false", "4|false", "5|false"],
        "duplicates collapse into the set without changing the answer"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, id = ANY (ARRAY[3]), id <> ALL (ARRAY[3]) FROM an ORDER BY id"
        ),
        vec![
            "1|false|true",
            "2|false|true",
            "3|true|false",
            "4|false|true",
            "5|false|true",
        ]
    );
}

/// Operators other than `=`/`<>` keep the comparison but still build the
/// array once.
#[test]
fn round597_other_operators() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, id > ANY (ARRAY[1,4]), id < ALL (ARRAY[4,5]), id >= ALL (ARRAY[1,2]) \
             FROM an ORDER BY id"
        ),
        vec![
            "1|false|true|false",
            "2|true|true|true",
            "3|true|true|true",
            "4|true|false|true",
            "5|true|false|true",
        ]
    );
}

/// A right-hand side that genuinely varies per row cannot be built once,
/// and must keep answering the same.
#[test]
fn round597_non_constant_arrays_keep_the_interpreter() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM an WHERE id = ANY (ARRAY[id, 1]) ORDER BY id"
        ),
        vec!["1", "2", "3", "4", "5"],
        "the array names the row's own column"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM an WHERE id = ANY (ARRAY[b, b+1])"
        ),
        vec!["4"]
    );
}

/// In a filter, negated, and beside other predicates — and at a size where
/// the per-row rebuild was the whole cost.
#[test]
fn round597_filters_and_scale() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM an WHERE id = ANY (ARRAY[2,4]) ORDER BY id"
        ),
        vec!["2", "4"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM an WHERE NOT (id = ANY (ARRAY[2,4])) ORDER BY id"
        ),
        vec!["1", "3", "5"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM an WHERE id <> ALL (ARRAY[2,4]) ORDER BY id"
        ),
        vec!["1", "3", "5"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM an WHERE id = ANY (ARRAY[1,2,3]) AND s IS NOT NULL ORDER BY id"
        ),
        vec!["1", "2"]
    );
    let mut e2 = Engine::new();
    e2.execute("CREATE TABLE big (id INT)").unwrap();
    e2.execute("INSERT INTO big SELECT gg FROM generate_series(1, 20000) gg")
        .unwrap();
    // The set spelling and the IN spelling must agree at scale.
    assert_eq!(
        vals(
            &mut e2,
            "SELECT count(*) FROM big WHERE id = ANY (ARRAY[1,2,3,4,5,6,7,8,9,10])"
        ),
        vals(
            &mut e2,
            "SELECT count(*) FROM big WHERE id IN (1,2,3,4,5,6,7,8,9,10)"
        )
    );
    assert_eq!(
        vals(
            &mut e2,
            "SELECT count(*) FROM big WHERE id <> ALL (ARRAY[1,2,3,4,5])"
        ),
        vec!["19995"]
    );
}
