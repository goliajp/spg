//! v7.39 (round 646) — writing through an inheritance parent, and the
//! `ONLY` that makes it expressible.
//!
//! Round 645 landed the feature and REFUSED `UPDATE` / `DELETE` through
//! the parent, because fanning out to the children is only half the
//! statement — the parent holds rows of its own — and running the
//! ordinary single-table body would quietly miss every child row.
//!
//! What was missing was a way to say "the parent's own rows" as a
//! statement. Round 644 taught the FROM clause `ONLY` and left DML
//! behind: it needed a field on `UpdateStatement`, which carries a
//! warning that round 413 measured widening it in place overflowing the
//! parser's nesting stack. That warning was about `from_sources`, a
//! struct wide enough to need boxing; a `bool` lands in padding already
//! there. With it, the parent is just another term of the fan-out —
//! carried as ONLY, which is exactly what stops the recursive call
//! re-entering the fan-out forever.
//!
//! So one field closes two things: PG's `UPDATE ONLY` / `DELETE FROM
//! ONLY` spellings, and the write path through an inheritance parent.
//!
//! CHECK constraints inherit in the same round, name included. An
//! unnamed CHECK is auto-named per table, so copying it as-is gave the
//! child `<child>_a_check` where PG reports the parent's
//! `<parent>_a_check` — and the violation message is where a user meets
//! the name. Measured on PG18 both ways.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
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
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

fn family() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE par (a INT NOT NULL, b TEXT DEFAULT 'd')")
        .unwrap();
    e.execute("CREATE TABLE ch (c BOOL) INHERITS (par)").unwrap();
    e.execute("INSERT INTO par (a) VALUES (1)").unwrap();
    e.execute("INSERT INTO ch (a, c) VALUES (2, true)").unwrap();
    e
}

#[test]
fn round646_update_through_the_parent_reaches_the_child() {
    let mut e = family();
    e.execute("UPDATE par SET b = 'u' WHERE a = 2").unwrap();
    assert_eq!(one(&mut e, "SELECT b FROM ch"), "u");
    // …and the parent's own row when the WHERE picks it.
    e.execute("UPDATE par SET b = 'p' WHERE a = 1").unwrap();
    assert_eq!(one(&mut e, "SELECT b FROM ONLY par"), "p");
}

#[test]
fn round646_update_only_stops_at_the_parent() {
    let mut e = family();
    e.execute("UPDATE ONLY par SET b = 'o'").unwrap();
    assert_eq!(one(&mut e, "SELECT b FROM ONLY par"), "o");
    // The child is untouched.
    assert_eq!(one(&mut e, "SELECT b FROM ch"), "d");
}

#[test]
fn round646_delete_through_the_parent_reaches_the_child() {
    let mut e = family();
    e.execute("DELETE FROM par WHERE a = 2").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM ch"), "0");
    assert_eq!(one(&mut e, "SELECT count(*) FROM ONLY par"), "1");
    // Unqualified, it takes everything.
    e.execute("DELETE FROM par").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM par"), "0");
}

#[test]
fn round646_delete_only_leaves_the_child_alone() {
    let mut e = family();
    e.execute("DELETE FROM ONLY par").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM ONLY par"), "0");
    assert_eq!(one(&mut e, "SELECT count(*) FROM ch"), "1");
    assert_eq!(one(&mut e, "SELECT count(*) FROM par"), "1");
}

/// A partition parent holds no rows, so ONLY there is a way of asking
/// for nothing — which is what PG answers.
#[test]
fn round646_only_on_a_partition_parent_writes_nothing() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE po (k INT, v TEXT) PARTITION BY RANGE (k)")
        .unwrap();
    e.execute("CREATE TABLE po1 PARTITION OF po FOR VALUES FROM (0) TO (10)")
        .unwrap();
    e.execute("INSERT INTO po VALUES (1, 'a')").unwrap();
    e.execute("UPDATE ONLY po SET v = 'z'").unwrap();
    assert_eq!(one(&mut e, "SELECT v FROM po"), "a");
    e.execute("DELETE FROM ONLY po").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM po"), "1");
    // Without ONLY both reach the partition.
    e.execute("UPDATE po SET v = 'z'").unwrap();
    assert_eq!(one(&mut e, "SELECT v FROM po"), "z");
    e.execute("DELETE FROM po").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM po"), "0");
}

#[test]
fn round646_a_check_inherits_with_its_name() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE cp (a INT, CHECK (a > 0))").unwrap();
    e.execute("CREATE TABLE ci () INHERITS (cp)").unwrap();
    // Catalogued…
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM pg_constraint \
             WHERE conrelid = (SELECT oid FROM pg_class WHERE relname = 'ci') AND contype = 'c'"
        ),
        "1"
    );
    // …and enforced, under the PARENT's name, which is what PG reports.
    e.execute("INSERT INTO ci VALUES (5)").unwrap();
    let err = e
        .execute("INSERT INTO ci VALUES (-1)")
        .expect_err("the inherited check must bite");
    assert!(
        err.to_string().contains("cp_a_check"),
        "the name travels with the constraint: {err}"
    );
    assert_eq!(one(&mut e, "SELECT count(*) FROM ci"), "1");
}

/// A table genuinely called `only` still works — the keyword is only a
/// keyword when another identifier follows it.
#[test]
fn round646_a_table_named_only_is_not_the_keyword() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE only (a INT)").unwrap();
    e.execute("INSERT INTO only VALUES (1)").unwrap();
    e.execute("UPDATE only SET a = 2").unwrap();
    assert_eq!(one(&mut e, "SELECT a FROM only"), "2");
    e.execute("DELETE FROM only").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM only"), "0");
}
