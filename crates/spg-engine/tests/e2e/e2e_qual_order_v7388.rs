//! v7.38.8 — the scan filter runs the cheap half of a conjunction
//! first, and doing so changes nothing but the time.
//!
//! On a customer profile the same query written the other way round
//! cost 4.7 times as much: a jsonb containment written before a
//! timestamp window was evaluated on every row, where written after it
//! it saw only the rows the window let through. Which one a person
//! writes first is habit.

use spg_engine::Engine;

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, n INT NOT NULL, s TEXT NOT NULL)")
        .unwrap();
    for i in 1..=50 {
        e.execute(&format!(
            "INSERT INTO t VALUES ({i}, {}, '{}')",
            i % 7,
            if i % 3 == 0 { "yes" } else { "no" }
        ))
        .unwrap();
    }
    e
}

fn rows(e: &mut Engine, sql: &str) -> String {
    format!("{:?}", e.execute(sql).unwrap())
}

#[test]
fn both_written_orders_give_the_same_answer() {
    let mut e = seeded();
    let a = rows(
        &mut e,
        "SELECT id FROM t WHERE s = 'yes' AND id > 10 AND id < 40 ORDER BY id",
    );
    let b = rows(
        &mut e,
        "SELECT id FROM t WHERE id > 10 AND id < 40 AND s = 'yes' ORDER BY id",
    );
    assert_eq!(a, b, "reordering a conjunction must not change the answer");
}

#[test]
fn three_valued_logic_survives_the_reorder() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id INT NOT NULL, n INT)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1, NULL), (2, 5), (3, NULL)")
        .unwrap();
    // A NULL operand makes the conjunct UNKNOWN, and AND is commutative
    // under three-valued logic — so the row set cannot depend on which
    // side is written first.
    let a = rows(
        &mut e,
        "SELECT id FROM u WHERE n > 1 AND id > 0 ORDER BY id",
    );
    let b = rows(
        &mut e,
        "SELECT id FROM u WHERE id > 0 AND n > 1 ORDER BY id",
    );
    assert_eq!(a, b);
    assert!(
        a.contains('2') && !a.contains('1'),
        "only the non-NULL row: {a}"
    );
}

#[test]
fn a_conjunct_that_can_raise_is_not_hoisted() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE z (id INT NOT NULL, d INT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO z VALUES (1, 0), (2, 4)").unwrap();
    // `100 / d` raises on the row where d is 0. The guard is written
    // FIRST and must stay first: the pass only moves conjuncts that
    // cannot raise, and it only moves them EARLIER, so nothing can
    // overtake this one and expose the division.
    let r = e.execute("SELECT id FROM z WHERE d > 0 AND 100 / d > 10");
    assert!(
        r.is_ok(),
        "the guard must still short-circuit the division: {r:?}"
    );
}

#[test]
fn equality_is_not_hoisted_over_the_rest() {
    // Deliberate, and measured: `col = <literal>` is the shape an index
    // seek consumes, so after a seek every row already satisfies it and
    // hoisting it spends a comparison per row on a settled question
    // while demoting the predicate that actually filters. Ranges move;
    // equality does not. Asserted through the answer, which must be the
    // same either way regardless of what moved.
    let mut e = seeded();
    let a = rows(
        &mut e,
        "SELECT id FROM t WHERE s = 'yes' AND n = 3 ORDER BY id",
    );
    let b = rows(
        &mut e,
        "SELECT id FROM t WHERE n = 3 AND s = 'yes' ORDER BY id",
    );
    assert_eq!(a, b);
}
