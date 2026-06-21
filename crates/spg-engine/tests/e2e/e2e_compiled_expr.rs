//! v7.32 (executor architecture v2, P1) — compiled-expression
//! semantics. The compiled WHERE path (Step::InSet / Step::Like and
//! the flat step machine) must be bit-for-bit with the interpreter,
//! including SQL three-valued logic, NULL propagation, cross-family
//! IN coercion, and [I]LIKE. These shapes drive a WHERE big enough
//! to take the compiled path; correctness is the gate.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn ids(rs: &[Vec<Value<'static>>]) -> Vec<i64> {
    rs.iter()
        .map(|r| match r[0] {
            Value::Int(n) => i64::from(n),
            Value::BigInt(n) => n,
            ref o => panic!("not int: {o:?}"),
        })
        .collect()
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT, n INT, s TEXT)").unwrap();
    e.execute(
        "INSERT INTO t VALUES (1,10,'alpha'),(2,20,'BETA'),(3,NULL,'gamma'),\
         (4,40,NULL),(5,50,'delta')",
    )
    .unwrap();
    e
}

/// A large all-literal IN list (forces the InSet compile product)
/// combined with another predicate so the whole WHERE compiles.
fn big_in(extra: &str, set: &str) -> String {
    // 70 literals (> the set threshold) including the values of
    // interest so the membership answer is exercised.
    let mut lits: Vec<String> = (100..170).map(|x| x.to_string()).collect();
    lits.extend(set.split(',').map(str::to_string));
    format!(
        "SELECT id FROM t WHERE n IN ({}) {extra} ORDER BY id",
        lits.join(",")
    )
}

#[test]
fn inset_membership_and_3vl_null() {
    let mut e = seeded();
    // n IN {10,40}: row1(10) row4(40); row3 has NULL n → never matches.
    assert_eq!(ids(&rows(&mut e, &big_in("", "10,40"))), vec![1, 4]);
    // NOT IN: NULL n yields NULL (excluded); matches excluded too.
    let q = big_in("", "10,40").replace("n IN (", "n NOT IN (");
    assert_eq!(ids(&rows(&mut e, &q)), vec![2, 5]);
}

#[test]
fn inset_null_in_list_makes_nonmatch_null() {
    let mut e = seeded();
    // List carries NULL → a non-matching, non-null needle yields
    // NULL (filtered out), not FALSE. Only the true matches survive.
    let mut lits: Vec<String> = (100..170).map(|x| x.to_string()).collect();
    lits.push("NULL".into());
    lits.push("20".into());
    let q = format!(
        "SELECT id FROM t WHERE n IN ({}) ORDER BY id",
        lits.join(",")
    );
    assert_eq!(ids(&rows(&mut e, &q)), vec![2]);
}

#[test]
fn inset_text_family() {
    let mut e = seeded();
    let mut lits: Vec<String> = (0..70).map(|x| format!("'x{x}'")).collect();
    lits.push("'gamma'".into());
    lits.push("'delta'".into());
    let q = format!(
        "SELECT id FROM t WHERE s IN ({}) ORDER BY id",
        lits.join(",")
    );
    assert_eq!(ids(&rows(&mut e, &q)), vec![3, 5]);
}

#[test]
fn compiled_like_and_ilike() {
    let mut e = seeded();
    // LIKE compiles (literal pattern); combine with a compilable
    // predicate so the whole WHERE takes the compiled path.
    assert_eq!(
        ids(&rows(
            &mut e,
            "SELECT id FROM t WHERE s LIKE 'a%' AND id < 100 ORDER BY id"
        )),
        vec![1]
    );
    // ILIKE case-folds both sides.
    assert_eq!(
        ids(&rows(
            &mut e,
            "SELECT id FROM t WHERE s ILIKE 'beta' AND id < 100 ORDER BY id"
        )),
        vec![2]
    );
    // NOT LIKE; NULL s propagates to NULL (excluded).
    assert_eq!(
        ids(&rows(
            &mut e,
            "SELECT id FROM t WHERE s NOT LIKE 'a%' AND id < 100 ORDER BY id"
        )),
        vec![2, 3, 5]
    );
}

#[test]
fn compiled_binary_and_isnull_and_collation() {
    let mut e = seeded();
    // Flat AND spine, comparisons, IS NULL — all compiled.
    assert_eq!(
        ids(&rows(
            &mut e,
            "SELECT id FROM t WHERE n >= 20 AND n <= 50 AND s IS NOT NULL ORDER BY id"
        )),
        vec![2, 5]
    );
    assert_eq!(
        ids(&rows(
            &mut e,
            "SELECT id FROM t WHERE n IS NULL ORDER BY id"
        )),
        vec![3]
    );
}
