//! Round 762 (F31-C2) — string_agg's separator is PER ROW in PG,
//! PG18-measured (round-761 audit, claims 49-53): element i is
//! prefixed by ITS row's separator, a NULL separator renders empty,
//! a skipped-NULL value row's separator is never used, and the pair
//! travels through the aggregate-internal ORDER BY. SPG snapshotted
//! the last row's separator and used it everywhere — silent-wrong for
//! every non-constant separator (`a<c>b<c>c` where PG answers
//! `a<b>b<c>c`) — under comments that claimed the snapshot WAS PG's
//! behaviour.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
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
        other => panic!("{other:?}"),
    }
}

#[test]
fn round762_per_row_separator_answers_as_pg() {
    let mut e = Engine::new();
    for (sql, want) in [
        (
            "SELECT string_agg(v, '<' || v || '>') FROM (VALUES ('a'),('b'),('c')) t(v)",
            "a<b>b<c>c",
        ),
        // NULL separator renders empty.
        (
            "SELECT string_agg(v, CASE WHEN v = 'b' THEN NULL ELSE '-' END) \
             FROM (VALUES ('a'),('b'),('c')) t(v)",
            "ab-c",
        ),
        // A skipped-NULL value row's separator is never used.
        (
            "SELECT string_agg(v, sep) FROM (VALUES ('a','1'),(NULL,'2'),('c','3')) t(v, sep)",
            "a3c",
        ),
        // The pair travels through the aggregate-internal ORDER BY.
        (
            "SELECT string_agg(v, sep ORDER BY v DESC) \
             FROM (VALUES ('a','1'),('b','2'),('c','3')) t(v, sep)",
            "c2b1a",
        ),
        (
            "SELECT string_agg(v, sep ORDER BY v) \
             FROM (VALUES ('b','B'),('a','A'),('c','C')) t(v, sep)",
            "aBbCc",
        ),
        // Constant separators and DISTINCT keep their shapes.
        ("SELECT string_agg(v, ', ') FROM (VALUES ('x'),('y')) t(v)", "x, y"),
        (
            "SELECT string_agg(DISTINCT v, '-') FROM (VALUES ('b'),('a'),('b')) t(v)",
            "a-b",
        ),
        // Per group.
        (
            "SELECT g, string_agg(v, '[' || v || ']') \
             FROM (VALUES (1,'a'),(1,'b'),(2,'c'),(2,'d')) t(g, v) GROUP BY g ORDER BY g",
            "1|a[b]b;2|c[d]d",
        ),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}
