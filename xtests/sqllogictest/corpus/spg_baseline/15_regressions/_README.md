# 15_regressions — 历史 regression 归属

> Per PG `src/test/regress/100_bugs.pl` convention (referenced in
> `docs/TESTING_V2_SKELETON.md` 工艺纪律 #4): each shipped bug-fix
> gets one corpus test file. A future commit that silently
> re-introduces the regression fails this file.
>
> Naming: `round_NN_shortname.test` for mailrs round series
> (v7.30.x and on), `kNN_shortname.test` for cascade root causes
> (v7.37.x K-series).

## Index

| File | Round / fix | What broke | What this corpus pins |
|---|---|---|---|
| `round_14_text_jumbo.test` | v7.23.0 round-14 | TEXT codec used u16 length prefix → TEXT > 64 KiB panicked at write time + bricked the column. Hit prod on mailrs `text_body` for long emails | 70 KB / 80 KB body round-trips through INSERT/UPDATE/SELECT without truncation |
| `round_20_typed_agg_columns.test` | v7.26.0 round-20 | Aggregate / expression columns returned generic TEXT instead of underlying widened type; missing-column refs silently returned 0 rows | SUM/COUNT/expression-column types preserved; missing-column refs surface as statement error |
| `round_25_in_list_flat.test` | v7.30.2 round-25 | `IN (subquery)` materialised inner result as left-deep OR-Eq chain → expression depth ∝ inner row count → stack overflow on 24k-row catalog | `IN (...)` lowers to flat `Expr::InList`; query against 1k-row inner works without deep recursion |
| `round_27_returning_type.test` | v7.31.0 round-27 | `RETURNING expr` returned everything as TEXT — type info wasn't carried over from the SELECT-list inference path | RETURNING preserves the underlying column type for casts (INT stays INT, BIGINT stays BIGINT) |
| `round_28_update_correlated.test` | v7.31.x round-28 | `UPDATE t SET c = (SELECT ... WHERE inner.col = t.col)` lost the target row binding mid-correlation | Correlated subquery in UPDATE SET sees the current outer row |
| `round_29_filter_clause.test` | v7.32 round-29 | `agg(args) FILTER (WHERE cond)` parser support missing | The PG-standard FILTER aggregate modifier parses + executes |
| `k02_in_list_visitor.test` | v7.37.7 K02 | `visit_expr_columns_and_subqueries` missed the `Expr::InList` arm → `pull_up_exists_sublinks` bailed on `inner.col IN (literals)` shape → mailrs Class B cascade × 4 recurrences | EXISTS over `inner.col IN (literals)` doesn't degrade to per-outer-row materialisation; result rows correct |

(Earlier rounds 12-24 have engine-level tests in
`crates/spg-engine/tests/e2e/` but no corpus entry yet; defer to a
v7.38 sweep once the framework here is established.)
