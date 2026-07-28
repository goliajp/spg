//! v7.39 (round 604) — the other spelling of `= ANY (<constant array>)`
//! still walked the array.
//!
//! Round 597 gave `x = ANY (ARRAY[1,2,3])` the membership set an IN list
//! builds, taking it from 268 ms over 500k rows to 1.93. It could not do the
//! same for `x = ANY ('{1,2,3}'::int[])`: it built the set from AST
//! literals, and that spelling keeps its elements inside a string. So the
//! array was folded once and every row still walked it, and the shape stayed
//! at 43.49 ms against PG18's 9.37.
//!
//! By the time the compiler asks, the array has been evaluated — the
//! elements are values, right there. Building the set from those closes the
//! last equality spelling:
//!
//!     id = ANY ('{1..10}'::INT[])   43.49 -> 1.87 ms   PG 7.21   4.64x -> 0.26x
//!
//! and all six equality / inequality spellings now beat PG:
//!
//!     id = ANY (ARRAY[1..10])        1.92   PG 8.33
//!     id IN (1..10)                  1.93   PG 7.50
//!     id = ANY (ARRAY[1])            1.86   PG 5.64
//!     id = ANY (ARRAY[1..20])        1.97   PG 7.79
//!     id <> ALL (ARRAY[1..5])        3.25   PG 9.46
//!     s = ANY (ARRAY['row1',…])      1.97   PG 8.76
//!
//! `id > ANY (ARRAY[1,2,3])` stays on the folded-array walk at 17.28 against
//! 7.60 — it is a comparison, not a membership test, and no set answers it.
//!
//! What the pins are for. The set is built from VALUES now rather than from
//! literal syntax, so the families it accepts have to be the ones it can
//! answer exactly: an integer set is right across widths (`INT` column
//! against a `BIGINT[]` and the reverse), a text set compares verbatim, and
//! anything else — a NUMERIC array, a mixed one — has to fall back rather
//! than answer approximately. The three-valued rules are the ones a NULL
//! element and a NULL left-hand side impose, and an empty array is still
//! decided by emptiness alone. All 16 shapes here were checked against live
//! PG18 and matched.

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
    e.execute("CREATE TABLE ac (id INT, b BIGINT, s TEXT)").unwrap();
    e.execute("INSERT INTO ac VALUES (1,1,'a'),(2,2,'b'),(3,NULL,NULL),(4,4,'d'),(5,5,'e')")
        .unwrap();
    e
}

/// The cast spelling, and the three-valued rules it has to keep.
#[test]
fn round604_cast_array_membership() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT id, id = ANY ('{1,3,5}'::INT[]) FROM ac ORDER BY id"),
        vec!["1|true", "2|false", "3|true", "4|false", "5|true"]
    );
    assert_eq!(
        vals(&mut e, "SELECT id, id = ANY ('{1,NULL,5}'::INT[]) FROM ac ORDER BY id"),
        vec!["1|true", "2|NULL", "3|NULL", "4|NULL", "5|true"],
        "a NULL element makes a non-match NULL, not false"
    );
    assert_eq!(
        vals(&mut e, "SELECT id, id <> ALL ('{1,3}'::INT[]) FROM ac ORDER BY id"),
        vec!["1|false", "2|true", "3|false", "4|true", "5|true"]
    );
    assert_eq!(
        vals(&mut e, "SELECT id, id <> ALL ('{1,NULL}'::INT[]) FROM ac ORDER BY id"),
        vec!["1|false", "2|NULL", "3|NULL", "4|NULL", "5|NULL"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, b = ANY ('{1,2}'::INT[]), b <> ALL ('{1,2}'::INT[]) FROM ac ORDER BY id"
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
            "SELECT id, id = ANY ('{}'::INT[]), id <> ALL ('{}'::INT[]) FROM ac ORDER BY id"
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
    assert_eq!(
        vals(&mut e, "SELECT id, id = ANY ('{2,2,2}'::INT[]) FROM ac ORDER BY id"),
        vec!["1|false", "2|true", "3|false", "4|false", "5|false"],
        "duplicates collapse into the set without changing the answer"
    );
}

/// The families the set may answer, and the ones it must hand back.
#[test]
fn round604_element_families() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT id, s = ANY ('{a,d}'::TEXT[]) FROM ac ORDER BY id"),
        vec!["1|true", "2|false", "3|NULL", "4|true", "5|false"]
    );
    assert_eq!(
        vals(&mut e, "SELECT id, b = ANY ('{1,4}'::INT[]) FROM ac ORDER BY id"),
        vec!["1|true", "2|false", "3|NULL", "4|true", "5|false"],
        "a BIGINT column against an INT array"
    );
    assert_eq!(
        vals(&mut e, "SELECT id, id = ANY ('{1,4}'::BIGINT[]) FROM ac ORDER BY id"),
        vec!["1|true", "2|false", "3|false", "4|true", "5|false"],
        "and the reverse"
    );
    assert_eq!(
        vals(&mut e, "SELECT id, id = ANY ('{1.0,4.0}'::NUMERIC[]) FROM ac ORDER BY id"),
        vec!["1|true", "2|false", "3|false", "4|true", "5|false"],
        "a NUMERIC array is not a family the set answers, so it keeps the walk"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, id > ANY ('{1,4}'::INT[]), id < ALL ('{4,5}'::INT[]) FROM ac ORDER BY id"
        ),
        vec![
            "1|false|true",
            "2|true|true",
            "3|true|true",
            "4|true|false",
            "5|true|false",
        ],
        "a comparison is not a membership test"
    );
}

/// The spellings must agree with each other, row by row, NULLs and all.
#[test]
fn round604_spellings_agree() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM ac WHERE (id = ANY ('{1,NULL,5}'::INT[])) \
             IS NOT DISTINCT FROM (id = ANY (ARRAY[1,NULL,5]))"
        ),
        vec!["5"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM ac WHERE (id = ANY ('{1,3}'::INT[])) \
             IS NOT DISTINCT FROM (id IN (1,3))"
        ),
        vec!["5"]
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM ac WHERE id = ANY ('{2,4}'::INT[]) ORDER BY id"),
        vec!["2", "4"]
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM ac WHERE NOT (id = ANY ('{2,4}'::INT[])) ORDER BY id"),
        vec!["1", "3", "5"]
    );
}

/// At a size where walking the array was the cost.
#[test]
fn round604_scale() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE big (id INT)").unwrap();
    e.execute("INSERT INTO big SELECT gg FROM generate_series(1, 20000) gg")
        .unwrap();
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM big WHERE id = ANY ('{1,2,3,4,5,6,7,8,9,10}'::INT[])"),
        vals(&mut e, "SELECT count(*) FROM big WHERE id IN (1,2,3,4,5,6,7,8,9,10)")
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM big WHERE id <> ALL ('{1,2,3,4,5}'::INT[])"),
        vec!["19995"]
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM big WHERE id = ANY ('{99999}'::INT[])"),
        vec!["0"]
    );
}
