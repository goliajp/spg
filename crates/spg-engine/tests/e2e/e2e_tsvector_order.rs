//! v7.38 (read01, T12.4) — tsvector ordering operators (<, <=, >, >=). PG's
//! tsvector comparison operators order by lexeme count first, then per-lexeme by
//! word (length, then bytes) — distinct from the btree/ORDER BY order (a
//! documented PG quirk). Equality stays position/weight sensitive.
//! Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn b(e: &mut Engine, sql: &str) -> bool {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => {
            matches!(rows[0].values[0], spg_storage::Value::Bool(true))
        }
        _ => panic!("rows"),
    }
}

#[test]
fn tsvector_ordering_operators() {
    let mut e = Engine::new();
    // Per-lexeme word order is length-first, then bytes.
    assert!(b(&mut e, "SELECT 'a b'::tsvector < 'a c'::tsvector"));
    assert!(!b(&mut e, "SELECT 'ab'::tsvector < 'b'::tsvector")); // len 2 > len 1
    assert!(b(&mut e, "SELECT 'cat'::tsvector < 'apple'::tsvector")); // len 3 < len 5
    assert!(b(&mut e, "SELECT 'aa'::tsvector < 'ab'::tsvector")); // same len, bytes
    // Lexeme count is compared first.
    assert!(b(&mut e, "SELECT 'z'::tsvector < 'a b'::tsvector")); // 1 lexeme < 2
    assert!(b(&mut e, "SELECT 'a'::tsvector < 'a b'::tsvector"));
    assert!(!b(&mut e, "SELECT 'b'::tsvector < 'a'::tsvector"));
    // <= / >= / >.
    assert!(b(&mut e, "SELECT 'a b'::tsvector >= 'a b'::tsvector"));
    assert!(b(&mut e, "SELECT 'ab'::tsvector > 'b'::tsvector")); // len 2 > len 1
    // Equality stays structural (position sensitive).
    assert!(b(&mut e, "SELECT 'a'::tsvector = 'a'::tsvector"));
    assert!(!b(&mut e, "SELECT 'a:1'::tsvector = 'a:2'::tsvector"));
}
