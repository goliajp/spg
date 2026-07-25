//! read01 round 472 (C15) — MySQL's `GROUP BY … WITH ROLLUP`.
//!
//! A parse error until this round, and there is no client-side rewrite: the
//! obvious one, `GROUP BY ROLLUP(…)`, is PG spelling that a MySQL client
//! cannot send, and MariaDB refuses an ORDER BY next to a rollup (1221) so
//! the client cannot fix the row order either.
//!
//! SPG already had the whole grouping-sets machinery — `ROLLUP()`, `CUBE()`,
//! `GROUPING SETS` all worked. What was missing was the MySQL spelling and,
//! less obviously, the ROW ORDER: the union-of-grouping-sets expansion emits
//! every leaf first and then every subtotal, where MySQL interleaves each
//! group's subtotal right after its own rows. A report renderer walking the
//! result in order depends on that.
//!
//! Both oracles were measured (MariaDB 11 and MySQL 9.7). They agree on the
//! rows and the order and disagree on one thing: MySQL allows an ORDER BY
//! beside a rollup, MariaDB raises 1221. SPG allows it — refusing would
//! break the clients that can write it, and no MariaDB client can.

use spg_engine::{Engine, QueryResult};

fn my() -> Engine {
    let mut e = Engine::new();
    e.set_backslash_escapes(true);
    e.execute("CREATE TABLE s (region VARCHAR(8), product VARCHAR(8), amt INT)")
        .unwrap();
    e.execute("INSERT INTO s VALUES ('east','a',10),('east','b',20),('west','a',30),('west','b',40)")
        .unwrap();
    e
}

fn rows(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
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
            .join(";"),
        other => panic!("{sql} -> {other:?}"),
    }
}

#[test]
fn round472_one_key_rolls_up() {
    let mut e = my();
    assert_eq!(
        rows(&mut e, "SELECT region, SUM(amt) FROM s GROUP BY region WITH ROLLUP"),
        "east|30;west|70;NULL|100"
    );
}

#[test]
fn round472_two_keys_interleave_each_subtotal_after_its_own_rows() {
    // The order is the point. MariaDB 11 and MySQL 9.7 both give:
    //   east a 10 / east b 20 / east NULL 30 / west a 30 / west b 40 /
    //   west NULL 70 / NULL NULL 100
    let mut e = my();
    assert_eq!(
        rows(
            &mut e,
            "SELECT region, product, SUM(amt) FROM s GROUP BY region, product WITH ROLLUP"
        ),
        "east|a|10;east|b|20;east|NULL|30;west|a|30;west|b|40;west|NULL|70;NULL|NULL|100"
    );
}

#[test]
fn round472_a_data_null_is_not_the_rollup_null() {
    // Sorting on the key alone cannot express this, and it is why the
    // synthesised order goes through GROUPING(). MariaDB 11 puts the
    // DATA-NULL group where a plain GROUP BY puts it — first — and only
    // the rollup's own NULL last. Both print as NULL.
    let mut e = Engine::new();
    e.set_backslash_escapes(true);
    e.execute("CREATE TABLE n (g VARCHAR(8), amt INT)").unwrap();
    e.execute("INSERT INTO n VALUES ('a',1),(NULL,2),('b',3)")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT g, SUM(amt) FROM n GROUP BY g WITH ROLLUP"),
        "NULL|2;a|1;b|3;NULL|6"
    );
}

#[test]
fn round472_a_written_order_by_wins() {
    // MySQL 9.7 allows one and it overrides; SPG follows MySQL here rather
    // than MariaDB's 1221 refusal.
    let mut e = my();
    assert_eq!(
        rows(
            &mut e,
            "SELECT region, SUM(amt) FROM s GROUP BY region WITH ROLLUP ORDER BY region DESC"
        ),
        "NULL|100;west|70;east|30"
    );
}

#[test]
fn round472_aggregates_beside_the_rollup_still_work() {
    let mut e = my();
    assert_eq!(
        rows(
            &mut e,
            "SELECT region, COUNT(*), SUM(amt) FROM s GROUP BY region WITH ROLLUP"
        ),
        "east|2|30;west|2|70;NULL|4|100"
    );
}

#[test]
fn round472_postgres_sessions_do_not_get_the_mysql_spelling() {
    // `WITH ROLLUP` is not PG syntax; a PG session must still refuse it,
    // and PG's own `ROLLUP(…)` must still work there.
    let mut e = Engine::new();
    e.execute("CREATE TABLE s (region VARCHAR(8), amt INT)").unwrap();
    e.execute("INSERT INTO s VALUES ('east',10),('west',20)")
        .unwrap();
    assert!(
        e.execute("SELECT region, SUM(amt) FROM s GROUP BY region WITH ROLLUP")
            .is_err()
    );
    assert_eq!(
        rows(&mut e, "SELECT region, SUM(amt) FROM s GROUP BY ROLLUP(region)"),
        "east|10;west|20;NULL|30"
    );
}
