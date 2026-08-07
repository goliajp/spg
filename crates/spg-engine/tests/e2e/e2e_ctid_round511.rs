//! v7.39 (round 511) — `ctid`, PG's physical row identity.
//!
//! SPG had NO system columns at all: `ctid`, `xmin`, `xmax`, `cmin`, `cmax`
//! and `tableoid` every one answered "column does not exist". That takes out
//! the dedup idiom every PG user knows —
//!
//!   DELETE FROM t WHERE ctid NOT IN (SELECT min(ctid) FROM t GROUP BY key)
//!
//! — and `xmin`, which is what an ORM reads for optimistic locking. The
//! values were already there; the row header carries xmin/xmax and the scan
//! already yields each row's position. Nothing exposed them.
//!
//! `tid` is a real type rather than a two-field record, and that is the
//! whole point: the idiom needs `min()` over it, PG has no `min(record)`,
//! and a TEXT form would order `(0,10) < (0,2) < (0,9)` — the dedup would
//! keep the wrong row and delete the right ones.
//!
//! The DML paths and the other five system columns followed in round 512.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d (k INT)").unwrap();
    e
}

fn seed(e: &mut Engine, n: usize, k: i32) {
    for _ in 0..n {
        e.execute(&format!("INSERT INTO d VALUES ({k})")).unwrap();
    }
}

fn text(e: &mut Engine, sql: &str) -> String {
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
            .collect::<Vec<_>>()
            .join(" "),
        other => panic!("{sql}: {other:?}"),
    }
}

/// The column exists, numbers from 1, and reports its own type.
#[test]
fn round511_ctid_names_the_row() {
    let mut e = engine();
    seed(&mut e, 3, 1);
    assert_eq!(text(&mut e, "SELECT ctid FROM d"), "(0,1) (0,2) (0,3)");
    assert_eq!(text(&mut e, "SELECT pg_typeof(ctid) FROM d LIMIT 1"), "tid");
    assert_eq!(text(&mut e, "SELECT ctid::text FROM d LIMIT 1"), "(0,1)");
}

/// `*` never expands a system column, as PG's does not — including in the
/// mixed shape, which is the only one where it could go wrong.
#[test]
fn round511_wildcard_does_not_expand_ctid() {
    let mut e = engine();
    seed(&mut e, 1, 7);
    assert_eq!(text(&mut e, "SELECT * FROM d"), "7");
    assert_eq!(text(&mut e, "SELECT *, ctid FROM d"), "7|(0,1)");
}

/// A tid orders by block then offset. Past nine rows this is where a text
/// form would answer `(0,9)` and take the dedup idiom with it.
#[test]
fn round511_tid_orders_numerically_not_as_text() {
    let mut e = engine();
    seed(&mut e, 12, 1);
    assert_eq!(
        text(&mut e, "SELECT min(ctid), max(ctid) FROM d"),
        "(0,1)|(0,12)"
    );
    // Numerically `(0,2) < (0,10)` and `(0,10) > (0,9)`. Under a text form
    // BOTH would answer the other way round, which is the failure mode this
    // pins against.
    assert_eq!(
        text(
            &mut e,
            "SELECT '(0,2)'::tid < '(0,10)'::tid, '(0,10)'::tid > '(0,9)'::tid"
        ),
        "true|true"
    );
    assert_eq!(
        text(&mut e, "SELECT greatest('(0,12)'::tid, '(0,9)'::tid)"),
        "(0,12)"
    );
    // min/max, GREATEST/LEAST and ORDER BY each reach a DIFFERENT comparator
    // in this engine, and the aggregate one answered `Equal` for anything it
    // had no arm for — which is why `max(ctid)` used to answer `(0,1)`.
    assert_eq!(text(&mut e, "SELECT count(DISTINCT ctid) FROM d"), "12");
}

/// A ctid read earlier names the row again.
#[test]
fn round511_a_row_can_be_named_by_its_ctid() {
    let mut e = engine();
    seed(&mut e, 3, 5);
    e.execute("INSERT INTO d VALUES (99)").unwrap();
    assert_eq!(
        text(&mut e, "SELECT k FROM d WHERE ctid = '(0,4)'::tid"),
        "99"
    );
    assert_eq!(
        text(&mut e, "SELECT ctid = '(0,1)'::tid FROM d LIMIT 1"),
        "true"
    );
}

/// The SELECT half of the idiom `ctid` exists for. It needs the tid to
/// survive a subquery, which is its own materialisation path.
#[test]
fn round511_the_dedup_idiom_selects_correctly() {
    let mut e = engine();
    seed(&mut e, 12, 1);
    seed(&mut e, 2, 2);
    assert_eq!(
        text(
            &mut e,
            "SELECT count(*) FROM d WHERE ctid NOT IN (SELECT min(ctid) FROM d GROUP BY k)"
        ),
        "12"
    );
    // And the rows it names are the duplicates, not the keepers.
    assert_eq!(
        text(
            &mut e,
            "SELECT min(ctid) FROM d WHERE ctid NOT IN (SELECT min(ctid) FROM d GROUP BY k)"
        ),
        "(0,2)"
    );
}

/// The write half landed in round 512; see `e2e_system_columns_round512`.
#[test]
fn round511_the_dml_paths_carry_ctid() {
    let mut e = engine();
    seed(&mut e, 2, 1);
    e.execute("DELETE FROM d WHERE ctid = '(0,1)'::tid")
        .unwrap();
    assert_eq!(text(&mut e, "SELECT count(*) FROM d"), "1");
}
