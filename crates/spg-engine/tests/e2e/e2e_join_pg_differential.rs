//! v7.37.16 — JOIN edge-case PostgreSQL 18 differential corpus (10th sweep).
//!
//! Every expected value below is the live PostgreSQL 18 answer captured on the
//! mini bench container (`psql -tA`). Each multi-row join is rendered to a
//! single deterministic scalar with `string_agg(... ORDER BY ...)` and
//! `coalesce(...,'X'/'N'/'<empty>')` so NULL-fills and row counts are all
//! visible in one comparison string. The focus is the EDGE cases the biz e2e
//! INNER/LEFT queries never exercise: NULL join keys, FULL OUTER, RIGHT,
//! USING/NATURAL column-merge, ON-vs-WHERE outer semantics, anti/semi joins,
//! and NOT IN against a NULL-bearing subquery.
//!
//! This 10th sweep found ONE real correctness bug (P13): a derived table
//! (VALUES or subquery) with a `AS y(cols)` column-alias list, used as a
//! JOIN right operand, never applied the alias list — `y.a` was
//! unresolvable while `y.column1` worked. Fixed in join.rs; see the P13
//! comment. Every other supported-syntax case matched PG18 exactly,
//! including the classic traps: ON-vs-WHERE outer semantics (P20/P21) and
//! NOT IN against a NULL-bearing subquery collapsing to empty (P24). The
//! unsupported-grammar gaps (RIGHT / FULL OUTER / NATURAL / USING
//! column-merge) are pinned as a KNOWN-LIMITATION ledger in
//! `join_pg18_known_gaps` below.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn build() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE l (k int, v text)").unwrap();
    e.execute("INSERT INTO l VALUES (1,'a'),(2,'b'),(3,'c'),(NULL,'n')")
        .unwrap();
    e.execute("CREATE TABLE r (k int, w text)").unwrap();
    e.execute("INSERT INTO r VALUES (2,'B'),(3,'C'),(4,'D'),(NULL,'m')")
        .unwrap();
    e.execute("CREATE TABLE s3 (k int, z text)").unwrap();
    e.execute("INSERT INTO s3 VALUES (2,'ZZ'),(3,'YY'),(5,'QQ')")
        .unwrap();
    // v7.37.16 — disjoint-column table for the NATURAL-no-common-cols
    // (→ CROSS) differential. Shares no column name with `l`/`r`.
    e.execute("CREATE TABLE d (m int, p text)").unwrap();
    e.execute("INSERT INTO d VALUES (7,'x'),(8,'y')").unwrap();
    e
}

/// Extract the single scalar cell of a one-row/one-col result as PG `-tA`
/// would render it. Returns `Err("<kind>")` on parse/exec failure or shape
/// mismatch so the caller can record it as a divergence instead of crashing
/// on the first unsupported feature.
fn scalar(e: &mut Engine, sql: &str) -> Result<String, String> {
    let r = match e.execute(sql) {
        Ok(r) => r,
        Err(err) => return Err(format!("ERROR({err:?})")),
    };
    let QueryResult::Rows { rows, .. } = r else {
        return Err("ERROR(not-Rows)".to_string());
    };
    if rows.len() != 1 {
        return Err(format!("ERROR({} rows)", rows.len()));
    }
    Ok(match &rows[0].values[0] {
        Value::Null => "NULL".to_string(),
        Value::Text(s) => s.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::SmallInt(n) => n.to_string(),
        other => format!("ERROR(unexpected {other:?})"),
    })
}

/// The full corpus. `(label, sql, pg18_expected)`.
fn corpus() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        (
            "P01_inner_render",
            "SELECT coalesce(string_agg(l.k::text||':'||l.v||'-'||r.w, ',' ORDER BY l.k, r.w),'<empty>') FROM l JOIN r ON l.k=r.k",
            "2:b-B,3:c-C",
        ),
        (
            "P02_inner_count",
            "SELECT count(*) FROM l JOIN r ON l.k=r.k",
            "2",
        ),
        (
            "P03_inner_null_both",
            "SELECT count(*) FROM l JOIN r ON l.k=r.k WHERE l.k IS NULL",
            "0",
        ),
        (
            "P04_left_render",
            "SELECT coalesce(string_agg(coalesce(l.k::text,'N')||':'||l.v||'-'||coalesce(r.w,'X'), ',' ORDER BY l.v),'<empty>') FROM l LEFT JOIN r ON l.k=r.k",
            "1:a-X,2:b-B,3:c-C,N:n-X",
        ),
        (
            "P05_left_count",
            "SELECT count(*) FROM l LEFT JOIN r ON l.k=r.k",
            "4",
        ),
        (
            "P10_cross_count",
            "SELECT count(*) FROM l CROSS JOIN r",
            "16",
        ),
        (
            "P11_self_render",
            "SELECT coalesce(string_agg(a.v||'<'||b.v, ',' ORDER BY a.k, b.k),'<empty>') FROM l a JOIN l b ON a.k < b.k",
            "a<b,a<c,b<c",
        ),
        (
            "P12_self_count",
            "SELECT count(*) FROM l a JOIN l b ON a.k < b.k",
            "3",
        ),
        // P13 — multi-column ON over two VALUES-derived tables with
        // column-alias lists. Before v7.37.16 SPG raised
        // `ColumnNotFound { name: "y.a" }`: a derived table (VALUES or
        // subquery) used as a JOIN right operand never applied its
        // `AS y(a, b, c)` column-alias list, so `y.a` was unresolvable
        // (only the inner name `y.column1` worked). The FROM-primary
        // derived-table path applied the list correctly; the join
        // right-operand path did not. Fixed in join.rs (probe schema
        // now renamed positionally). PG18 answer: `1-c`.
        (
            "P13_multicol_render",
            "SELECT coalesce(string_agg(x.a||'-'||y.c, ',' ORDER BY x.a),'<empty>') FROM (VALUES (1,10,'a'),(2,20,'b')) AS x(a,b,lab) JOIN (VALUES (1,10,'c'),(2,99,'d')) AS y(a,b,c) ON x.a=y.a AND x.b=y.b",
            "1-c",
        ),
        (
            "P14_using_count",
            "SELECT count(*) FROM l JOIN r USING(k)",
            "2",
        ),
        (
            "P20_on_pred_keeps_left",
            "SELECT coalesce(string_agg(l.v||'-'||coalesce(r.w,'X'), ',' ORDER BY l.v),'<empty>') FROM l LEFT JOIN r ON l.k=r.k AND r.w='B'",
            "a-X,b-B,c-X,n-X",
        ),
        (
            "P21_where_pred_inner",
            "SELECT coalesce(string_agg(l.v||'-'||coalesce(r.w,'X'), ',' ORDER BY l.v),'<empty>') FROM l LEFT JOIN r ON l.k=r.k WHERE r.w='B'",
            "b-B",
        ),
        (
            "P22_anti_notexists",
            "SELECT coalesce(string_agg(l.v, ',' ORDER BY l.v),'<empty>') FROM l WHERE NOT EXISTS (SELECT 1 FROM r WHERE r.k=l.k)",
            "a,n",
        ),
        (
            "P23_semi_exists",
            "SELECT coalesce(string_agg(l.v, ',' ORDER BY l.v),'<empty>') FROM l WHERE EXISTS (SELECT 1 FROM r WHERE r.k=l.k)",
            "b,c",
        ),
        (
            "P24_notin_with_null",
            "SELECT coalesce(string_agg(l.v, ',' ORDER BY l.v),'<empty>') FROM l WHERE l.k NOT IN (SELECT k FROM r)",
            "<empty>",
        ),
        (
            "P25_notin_no_null",
            "SELECT coalesce(string_agg(l.v, ',' ORDER BY l.v),'<empty>') FROM l WHERE l.k NOT IN (SELECT k FROM r WHERE k IS NOT NULL)",
            "a",
        ),
        (
            "P26_in_subquery",
            "SELECT coalesce(string_agg(l.v, ',' ORDER BY l.v),'<empty>') FROM l WHERE l.k IN (SELECT k FROM r)",
            "b,c",
        ),
        (
            "P27_left_groupby_count",
            "SELECT coalesce(string_agg(v||':'||c::text, ',' ORDER BY v),'<empty>') FROM (SELECT l.v AS v, count(r.w) AS c FROM l LEFT JOIN r ON l.k=r.k GROUP BY l.v) q",
            "a:0,b:1,c:1,n:0",
        ),
        (
            "P28_three_table_render",
            "SELECT coalesce(string_agg(l.v||'-'||r.w||'-'||s3.z, ',' ORDER BY l.k),'<empty>') FROM l JOIN r ON l.k=r.k JOIN s3 ON l.k=s3.k",
            "b-B-ZZ,c-C-YY",
        ),
        (
            "P29_left_distinct",
            "SELECT count(DISTINCT l.k) FROM l LEFT JOIN r ON l.k=r.k",
            "3",
        ),
        (
            "P31_cross_null_render",
            "SELECT coalesce(string_agg(coalesce(l.k::text,'N')||coalesce(r.k::text,'N'), ',' ORDER BY coalesce(l.k,-1), coalesce(r.k,-1)),'<empty>') FROM l CROSS JOIN r",
            "NN,N2,N3,N4,1N,12,13,14,2N,22,23,24,3N,32,33,34",
        ),
        (
            "P32_left_join_or_on",
            "SELECT coalesce(string_agg(l.v||'-'||coalesce(r.w,'X'), ',' ORDER BY l.v, r.w),'<empty>') FROM l LEFT JOIN r ON l.k=r.k OR l.k=r.k-1",
            "a-B,b-B,b-C,c-C,c-D,n-X",
        ),
        // v7.37.16 — RIGHT JOIN (promoted from known_gaps). Keeps every
        // right (r) row; unmatched right rows NULL-fill the left cols.
        // Output column order is unchanged (l cols then r cols).
        (
            "R01_right_count",
            "SELECT count(*) FROM l RIGHT JOIN r ON l.k=r.k",
            "4",
        ),
        (
            "R02_right_render",
            "SELECT coalesce(string_agg(coalesce(l.k::text,'N')||':'||coalesce(l.v,'X')||'-'||r.w, ',' ORDER BY r.w),'<empty>') FROM l RIGHT JOIN r ON l.k=r.k",
            "2:b-B,3:c-C,N:X-D,N:X-m",
        ),
        // Multi-col ON over two VALUES-derived tables, RIGHT-driven.
        (
            "R03_right_multicol",
            "SELECT coalesce(string_agg(coalesce(x.a::text,'N')||'-'||y.c, ',' ORDER BY y.c, coalesce(x.a::text,'N')),'<empty>') FROM (VALUES (1,10,'a'),(2,20,'b')) AS x(a,b,lab) RIGHT JOIN (VALUES (1,10,'c'),(2,99,'d')) AS y(a,b,c) ON x.a=y.a AND x.b=y.b",
            "1-c,N-d",
        ),
        // RIGHT + WHERE filters the joined (incl. NULL-filled) output.
        (
            "R04_right_where",
            "SELECT coalesce(string_agg(coalesce(l.v,'X')||'-'||r.w, ',' ORDER BY r.w),'<empty>') FROM l RIGHT JOIN r ON l.k=r.k WHERE r.k >= 3",
            "c-C,X-D",
        ),
        // WHERE l.k IS NULL selects exactly the NULL-filled-left rows.
        (
            "R05_right_where_leftnull",
            "SELECT count(*) FROM l RIGHT JOIN r ON l.k=r.k WHERE l.k IS NULL",
            "2",
        ),
        // v7.37.16 — FULL OUTER JOIN (promoted from known_gaps). Keeps
        // every row from both sides; unmatched either side NULL-fills the
        // other side's cols.
        (
            "F01_full_count",
            "SELECT count(*) FROM l FULL OUTER JOIN r ON l.k=r.k",
            "6",
        ),
        (
            "F02_full_render",
            "SELECT coalesce(string_agg(coalesce(l.k::text,'N')||':'||coalesce(l.v,'X')||'-'||coalesce(r.w,'Y'), ',' ORDER BY coalesce(l.k,r.k), l.v, r.w),'<empty>') FROM l FULL OUTER JOIN r ON l.k=r.k",
            "1:a-Y,2:b-B,3:c-C,N:X-D,N:n-Y,N:X-m",
        ),
        // Tiebreak on integer keys (x.b/y.b), not text, to stay
        // collation-agnostic: the join produces {a-c, b-Y, X-d} either
        // way; a text tiebreak would sort under PG's en_US.utf8 locale
        // vs SPG's bytewise order and diverge on that axis alone.
        (
            "F03_full_multicol",
            "SELECT coalesce(string_agg(coalesce(x.lab,'X')||'-'||coalesce(y.c,'Y'), ',' ORDER BY coalesce(x.a,y.a), coalesce(x.b,y.b)),'<empty>') FROM (VALUES (1,10,'a'),(2,20,'b')) AS x(a,b,lab) FULL OUTER JOIN (VALUES (1,10,'c'),(2,99,'d')) AS y(a,b,c) ON x.a=y.a AND x.b=y.b",
            "a-c,b-Y,X-d",
        ),
        // FULL OUTER JOIN USING(k) — USING desugars to l.k=r.k; count(*)
        // does not need the merged-column projection (still deferred).
        (
            "F04_full_using_count",
            "SELECT count(*) FROM l FULL OUTER JOIN r USING(k)",
            "6",
        ),
        // NULLs in both join keys on both sides — every join-key NULL is
        // unmatched, so it surfaces on its own side with the other NULL.
        (
            "F05_full_null_render",
            "SELECT coalesce(string_agg(coalesce(l.k::text,'N')||coalesce(r.k::text,'N'), ',' ORDER BY coalesce(l.k,r.k) NULLS LAST, r.k NULLS LAST),'<empty>') FROM l FULL OUTER JOIN r ON l.k=r.k",
            "1N,22,33,N4,NN,NN",
        ),
        // ---- v7.37.16 — USING column-merge (promoted from known_gaps) ----
        // PG merges the USING join column into ONE unqualified output
        // column: `t1.k` for INNER/LEFT, `t2.k` for RIGHT,
        // COALESCE(t1.k,t2.k) for FULL. `SELECT *` puts the merged column
        // FIRST (before the tables' other cols), so `k/v/w` here proves
        // the single-k shape AND the column order.
        (
            "U01_using_star_render",
            "SELECT coalesce(string_agg(k::text||'/'||v||'/'||w, ',' ORDER BY k),'X') FROM l JOIN r USING(k)",
            "2/b/B,3/c/C",
        ),
        // Bare `SELECT k` is unambiguous under USING (was: ambiguous err).
        (
            "U02_using_k_inner",
            "SELECT coalesce(string_agg(k::text,',' ORDER BY k),'X') FROM l JOIN r USING(k)",
            "2,3",
        ),
        // LEFT USING: merged k = left (l) side — unmatched-left rows keep
        // l.k (NULL join key 'n' surfaces as N via the left col).
        (
            "U03_using_k_left",
            "SELECT coalesce(string_agg(coalesce(k::text,'N'),',' ORDER BY v),'X') FROM l LEFT JOIN r USING(k)",
            "1,2,3,N",
        ),
        // RIGHT USING: merged k = right (r) side — unmatched-right row 4
        // and the right NULL key surface via r.k.
        (
            "U04_using_k_right",
            "SELECT coalesce(string_agg(coalesce(k::text,'N'),',' ORDER BY w),'X') FROM l RIGHT JOIN r USING(k)",
            "2,3,4,N",
        ),
        // FULL USING: merged k = COALESCE(l.k, r.k) — matched rows collapse
        // to one k, unmatched either side keeps whichever side is non-NULL.
        (
            "U05_using_k_full",
            "SELECT coalesce(string_agg(coalesce(k::text,'N'),',' ORDER BY k NULLS LAST),'X') FROM l FULL JOIN r USING(k)",
            "1,2,3,4,N,N",
        ),
        // ---- v7.37.16 — NATURAL JOIN (promoted from known_gaps) ----
        // NATURAL = USING over every common column name (here `k`).
        (
            "N01_natural_count",
            "SELECT count(*) FROM l NATURAL JOIN r",
            "2",
        ),
        (
            "N02_natural_render",
            "SELECT coalesce(string_agg(k::text||'/'||v||'/'||w, ',' ORDER BY k),'X') FROM l NATURAL JOIN r",
            "2/b/B,3/c/C",
        ),
        (
            "N03_natural_left_count",
            "SELECT count(*) FROM l NATURAL LEFT JOIN r",
            "4",
        ),
        (
            "N04_natural_left_render",
            "SELECT coalesce(string_agg(coalesce(k::text,'N')||'/'||v||'/'||coalesce(w,'X'), ',' ORDER BY v),'X') FROM l NATURAL LEFT JOIN r",
            "1/a/X,2/b/B,3/c/C,N/n/X",
        ),
        // NATURAL with NO common columns → PG falls back to a CROSS join.
        // `d(m,p)` shares no column name with `l(k,v)` → 4×2 = 8 rows.
        (
            "N05_natural_nocommon_cross",
            "SELECT count(*) FROM l NATURAL JOIN d",
            "8",
        ),
    ]
}

#[test]
fn join_pg18_differential_corpus() {
    let mut e = build();
    let mut mismatches: Vec<String> = Vec::new();
    for (label, sql, expected) in corpus() {
        let got = match scalar(&mut e, sql) {
            Ok(s) => s,
            Err(kind) => kind,
        };
        if got != expected {
            mismatches.push(format!("  {label}: PG18=[{expected}] SPG=[{got}]"));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} JOIN divergence(s) vs PG18:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

/// Known join features SPG does not yet implement. Pinning the CURRENT
/// behaviour (an error) here means the day any of these lands, this test
/// breaks and forces the author to promote the case into the green
/// `corpus()` above with its live PG18 answer. This is a
/// KNOWN-LIMITATION ledger, not an assertion that the SPG behaviour is
/// correct.
///
/// v7.37.16 — RIGHT JOIN, FULL OUTER JOIN, and FULL OUTER JOIN USING
/// (count) were promoted earlier. This sweep promoted the LAST THREE
/// ledger entries into `corpus()`: `NATURAL JOIN` (N01/N02),
/// `NATURAL LEFT JOIN` (N03/N04), and `USING column-merge` (U01–U05,
/// plus the `SELECT *` shape asserted in `using_star_column_shape`).
/// The parser now recognises `NATURAL [kind] JOIN` and records the
/// `USING` column list; the engine resolves NATURAL common columns and
/// applies PG's column-merge (single unqualified output column, first in
/// `SELECT *`) via a statement rewrite. The ledger is currently EMPTY —
/// every JOIN differential this corpus exercises matches PG18.
#[test]
fn join_pg18_known_gaps() {
    let mut e = build();
    // (label, sql, PG18-would-return) — each currently errors in SPG.
    // EMPTY: all prior gaps promoted into corpus() as of v7.37.16.
    let gaps: &[(&str, &str, &str)] = &[];
    for (feat, sql, note) in gaps {
        let r = e.execute(sql);
        assert!(
            r.is_err(),
            "KNOWN-GAP '{feat}' now SUCCEEDS in SPG — promote it into the green corpus() \
             with its live PG18 answer. {note}. sql=[{sql}]"
        );
    }
}

/// v7.37.16 — `SELECT *` over a `USING` join yields ONE merged column
/// `k` (not two), positioned FIRST, followed by each table's other
/// columns with bare names: exactly PG's `[k, v, w]`.
#[test]
fn using_star_column_shape() {
    let mut e = build();
    let r = e.execute("SELECT * FROM l JOIN r USING(k)").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!("expected Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        ["k", "v", "w"],
        "USING(*) must merge k and emit [k,v,w]"
    );
}
