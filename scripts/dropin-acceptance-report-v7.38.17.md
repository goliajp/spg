# SPG drop-in acceptance report

- image: `goliakk/spg:7.38.17`
- panel cases: 66  (pass 66  / fail 0)

**Verdict: PASS — every probed PG dialect feature lands on this SPG image.**

## Cases

| Case | Status | First error (if FAIL) |
|---|:-:|---|
| `D-pre.1.to_tsvector` | ✅ | |
| `D-pre.1.match_plainto` | ✅ | |
| `D-pre.1.match_to_tsquery` | ✅ | |
| `D-pre.1.ts_rank` | ✅ | |
| `D-pre.1.phraseto_tsquery` | ✅ | |
| `D-pre.2.literal` | ✅ | |
| `D-pre.2.any` | ✅ | |
| `D-pre.2.array_length` | ✅ | |
| `D-pre.2.subscript` | ✅ | |
| `D-pre.2.array_agg` | ✅ | |
| `D-pre.2.unnest_projection` | ✅ | |
| `D-pre.3.hex_literal_pg_escape` | ✅ | |
| `D-pre.3.hex_literal_double_backslash` | ✅ | |
| `D-pre.3.cast_round_trip` | ✅ | |
| `D-pre.3.octet_length_with_cast` | ✅ | |
| `D-pre.4.reserved_col_key_unquoted` | ✅ | |
| `D-pre.4.reserved_col_key_quoted` | ✅ | |
| `D-pre.4.ivfflat_index` | ✅ | |
| `D-pre.4.hnsw_vector_cosine` | ✅ | |
| `D-pre.4.bigserial_inline_pk` | ✅ | |
| `D-pre.4.multi_col_index_create` | ✅ | |
| `D-pre.4.multi_col_index_seek` | ✅ | |
| `D-pre.5.table_name_contacts` | ✅ | |
| `type.bigint` | ✅ | |
| `type.timestamptz` | ✅ | |
| `type.json_jsonb` | ✅ | |
| `type.uuid_gen` | ✅ | |
| `type.numeric` | ✅ | |
| `type.bytea_pg_escape` | ✅ | |
| `stock.on_conflict_do_nothing` | ✅ | |
| `stock.on_conflict_do_update` | ✅ | |
| `stock.returning` | ✅ | |
| `stock.cte` | ✅ | |
| `stock.fk_cascade` | ✅ | |
| `stock.transaction_commit` | ✅ | |
| `stock.transaction_rollback` | ✅ | |
| `round12.upsert_via_unique_index` | ✅ | |
| `round12.bitwise_flag_math` | ✅ | |
| `round12.extract_epoch` | ✅ | |
| `round12.update_where_in_subquery` | ✅ | |
| `round13.serial_continuity_multirow` | ✅ | |
| `round13.inline_pk_enforces` | ✅ | |
| `round21.bytea_above_64k` | ✅ | |
| `round21.text_array_elem_above_64k` | ✅ | |
| `round14.text_above_64k` | ✅ | |
| `round15.string_to_array` | ✅ | |
| `round16.order_by_nulls_first` | ✅ | |
| `round16.array_agg_internal_order` | ✅ | |
| `round16.setweight_trigger_fires` | ✅ | |
| `round16.correlated_not_exists_join` | ✅ | |
| `round17.ilike` | ✅ | |
| `round17.distinct_agg_case_cast` | ✅ | |
| `round17.cte_chain` | ✅ | |
| `round19.correlated_scalar_in_group_by` | ✅ | |
| `round20.aggregate_group_composite` | ✅ | |
| `round26.backfill_keyset_offset_skips_orphan` | ✅ | |
| `round26.backfill_desc_limit_top_end` | ✅ | |
| `mysql.wire answers` | ✅ | |
| `mysql.case folds` | ✅ | |
| `mysql.trailing space is data` | ✅ | |
| `mysql.indexed equality` | ✅ | |
| `mysql.indexed IN` | ✅ | |
| `mysql.indexed ORDER BY` | ✅ | |
| `mysql.indexed join` | ✅ | |
| `fixture.mailrs-pg-extensions.sql` | ✅ | |
| `fixture.mailrs-init-schema-v1.7.142.sql` | ✅ | |

## Reproducer

```bash
scripts/dropin-acceptance.sh --image goliakk/spg:7.38.17 --port 25433
```
