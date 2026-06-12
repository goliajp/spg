//! v7.30.2 (mailrs round-25) — `IN (subquery)` used to materialise
//! the inner result set into a left-deep OR-Eq chain, so expression
//! depth scaled with the inner ROW COUNT. On a 24k-row catalog one
//! mailbox search overflowed the 2 MiB worker stack and aborted the
//! embedding host process. The materialisation now produces a flat
//! `Expr::InList`, whose eval and drop are both iterative.
//!
//! These tests run inside a deliberately small (512 KiB) stack so a
//! depth-proportional regression fails fast instead of only at 24k+
//! rows on a full-size stack.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn ok(eng: &mut Engine, sql: &str) -> QueryResult {
    eng.execute(sql)
        .unwrap_or_else(|e| panic!("{sql:?}: {e:?}"))
}

fn select_value(eng: &mut Engine, sql: &str) -> Value {
    match ok(eng, sql) {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .next()
            .map(|mut r| r.values.remove(0))
            .expect("at least one row"),
        other => panic!("expected Rows, got {other:?}"),
    }
}

/// Run `f` on a 512 KiB stack — small enough that any
/// depth-∝-row-count recursion in the 20k-row tests below
/// overflows immediately.
fn on_small_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(f)
        .expect("spawn")
        .join()
        .expect("join");
}

/// Seed `messages` with `n` rows: id = 1..=n, thread_id = id % 97,
/// body alternates between matching and non-matching text.
fn seed_messages(eng: &mut Engine, n: usize) {
    ok(
        eng,
        "CREATE TABLE messages (id BIGINT PRIMARY KEY, thread_id BIGINT, body TEXT)",
    );
    // One multi-row INSERT per 1000 rows keeps parse overhead flat.
    let mut i = 1usize;
    while i <= n {
        let hi = (i + 999).min(n);
        let mut sql = String::from("INSERT INTO messages VALUES ");
        for id in i..=hi {
            if id > i {
                sql.push(',');
            }
            let body = if id % 2 == 0 { "alpha invoice" } else { "beta" };
            sql.push_str(&format!("({id}, {}, '{body}')", id % 97));
        }
        ok(eng, &sql);
        i = hi + 1;
    }
}

#[test]
fn in_subquery_20k_rows_no_stack_overflow() {
    on_small_stack(|| {
        let mut eng = Engine::new();
        seed_messages(&mut eng, 20_000);
        // Inner subquery matches 10k rows; before the fix the
        // materialised OR chain was 10k deep and eval / Drop both
        // overflowed the stack.
        let v = select_value(
            &mut eng,
            "SELECT count(*) FROM messages \
             WHERE id IN (SELECT id FROM messages WHERE body LIKE '%invoice%')",
        );
        assert!(matches!(v, Value::BigInt(10_000)), "got {v:?}");
    });
}

#[test]
fn not_in_subquery_20k_rows_no_stack_overflow() {
    on_small_stack(|| {
        let mut eng = Engine::new();
        seed_messages(&mut eng, 20_000);
        let v = select_value(
            &mut eng,
            "SELECT count(*) FROM messages \
             WHERE id NOT IN (SELECT id FROM messages WHERE body LIKE '%invoice%')",
        );
        assert!(matches!(v, Value::BigInt(10_000)), "got {v:?}");
    });
}

#[test]
fn large_literal_in_list_no_stack_overflow() {
    on_small_stack(|| {
        let mut eng = Engine::new();
        seed_messages(&mut eng, 2_000);
        // 20k-element literal IN list — the parse-time path used to
        // build the same OR chain.
        let mut sql = String::from("SELECT count(*) FROM messages WHERE id IN (");
        for k in 1..=20_000 {
            if k > 1 {
                sql.push(',');
            }
            sql.push_str(&k.to_string());
        }
        sql.push(')');
        let v = select_value(&mut eng, &sql);
        assert!(matches!(v, Value::BigInt(2_000)), "got {v:?}");
    });
}

/// mailrs round-25 query shape end-to-end on small data: UNION-of-
/// matchers CTE, thread-expansion CTE via nested IN-subqueries, then
/// an aggregate projection over the join.
#[test]
fn round25_union_cte_search_shape() {
    on_small_stack(|| {
        let mut eng = Engine::new();
        seed_messages(&mut eng, 4_000);
        let sql = "WITH matched AS ( \
                     SELECT id FROM messages WHERE body LIKE '%invoice%' \
                     UNION SELECT id FROM messages WHERE body LIKE '%alpha%' \
                   ), \
                   cands AS ( \
                     SELECT m_all.id FROM messages m_all \
                      WHERE m_all.thread_id IN ( \
                        SELECT thread_id FROM messages \
                         WHERE id IN (SELECT id FROM matched)) \
                   ) \
                   SELECT count(DISTINCT m.thread_id), count(*) \
                     FROM messages m JOIN cands c ON c.id = m.id";
        match ok(&mut eng, sql) {
            QueryResult::Rows { rows, .. } => {
                let vals = &rows[0].values;
                // every even id matches; thread fan-out pulls all 97
                // threads, so cands = all 4000 rows.
                assert!(matches!(vals[0], Value::BigInt(97)), "got {vals:?}");
                assert!(matches!(vals[1], Value::BigInt(4_000)), "got {vals:?}");
            }
            other => panic!("expected Rows, got {other:?}"),
        }
    });
}

/// The ≥64-element membership-set fast path must agree with the
/// linear scan on 3VL, NOT IN, text family, and cross-type needles.
#[test]
fn in_set_fast_path_semantics() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (a BIGINT, s TEXT, f FLOAT)");
    ok(
        &mut eng,
        "INSERT INTO t VALUES (1, 'k1', 1.0), (70, 'k70', 70.0), (200, 'nope', 200.0), (NULL, NULL, NULL)",
    );
    // 100-element integer list hits the set path (threshold 64).
    let big_list: String = (1..=100)
        .map(|k| k.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let v = select_value(
        &mut eng,
        &format!("SELECT count(*) FROM t WHERE a IN ({big_list})"),
    );
    assert!(matches!(v, Value::BigInt(2)), "got {v:?}");
    // NOT IN over the same list: only 200 passes (NULL row → NULL).
    let v = select_value(
        &mut eng,
        &format!("SELECT count(*) FROM t WHERE a NOT IN ({big_list})"),
    );
    assert!(matches!(v, Value::BigInt(1)), "got {v:?}");
    // NULL inside a big list: non-matching needle → NULL, so only
    // the matching rows pass and NOT IN passes nothing.
    let v = select_value(
        &mut eng,
        &format!("SELECT count(*) FROM t WHERE a IN ({big_list}, NULL)"),
    );
    assert!(matches!(v, Value::BigInt(2)), "got {v:?}");
    let v = select_value(
        &mut eng,
        &format!("SELECT count(*) FROM t WHERE a NOT IN ({big_list}, NULL)"),
    );
    assert!(matches!(v, Value::BigInt(0)), "got {v:?}");
    // Text family.
    let text_list: String = (1..=100)
        .map(|k| format!("'k{k}'"))
        .collect::<Vec<_>>()
        .join(",");
    let v = select_value(
        &mut eng,
        &format!("SELECT count(*) FROM t WHERE s IN ({text_list})"),
    );
    assert!(matches!(v, Value::BigInt(2)), "got {v:?}");
    // Cross-family needle (FLOAT column vs integer list) falls back
    // to the coercing linear scan: 1.0 and 70.0 still match.
    let v = select_value(
        &mut eng,
        &format!("SELECT count(*) FROM t WHERE f IN ({big_list})"),
    );
    assert!(matches!(v, Value::BigInt(2)), "got {v:?}");
    // AND-spine: set-served IN combined with another predicate.
    let v = select_value(
        &mut eng,
        &format!("SELECT count(*) FROM t WHERE a IN ({big_list}) AND s = 'k70'"),
    );
    assert!(matches!(v, Value::BigInt(1)), "got {v:?}");
}

/// IN / NOT IN three-valued logic must survive the flat-list rewrite:
/// `x IN (…)` is NULL when nothing matched but a NULL was seen.
#[test]
fn in_list_three_valued_logic() {
    let mut eng = Engine::new();
    ok(&mut eng, "CREATE TABLE t (a INT)");
    ok(&mut eng, "INSERT INTO t VALUES (1), (2), (NULL)");
    // 2 IN (1, NULL, 2) → true
    let v = select_value(&mut eng, "SELECT count(*) FROM t WHERE a IN (1, NULL)");
    assert!(matches!(v, Value::BigInt(1)), "got {v:?}");
    // a NOT IN (1, NULL): a=2 → NULL (not true) — PG semantics: 0 rows pass
    let v = select_value(&mut eng, "SELECT count(*) FROM t WHERE a NOT IN (1, NULL)");
    assert!(matches!(v, Value::BigInt(0)), "got {v:?}");
    // empty-set semantics via subquery: x IN (∅) → false, x NOT IN (∅) → true
    let v = select_value(
        &mut eng,
        "SELECT count(*) FROM t WHERE a IN (SELECT a FROM t WHERE a > 100)",
    );
    assert!(matches!(v, Value::BigInt(0)), "got {v:?}");
    let v = select_value(
        &mut eng,
        "SELECT count(*) FROM t WHERE a NOT IN (SELECT a FROM t WHERE a > 100)",
    );
    // NOT IN empty list is TRUE for every row including… NULL rows?
    // PG: NULL NOT IN (empty) → true (no comparisons happen). 3 rows.
    assert!(matches!(v, Value::BigInt(3)), "got {v:?}");
}
