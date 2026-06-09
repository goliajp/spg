# SPG drop-in acceptance report

- image: `goliakk/spg:7.17.0`
- panel cases: 35  (pass 31  / fail 4)

**Verdict: FAIL — 4 case(s) below show real SPG dialect gaps. See the table.**

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
| `D-pre.3.hex_literal_pg_escape` | ❌ |  parse: parse error at token #8: expected ',' or ')' in VALUES tuple, got String("\\\\xdeadbeef") |
| `D-pre.3.hex_literal_double_backslash` | ✅ | |
| `D-pre.3.cast_round_trip` | ❌ |  parse: parse error at token #9: unsupported cast target `::bytea` |
| `D-pre.3.octet_length_with_cast` | ❌ |  parse: parse error at token #9: unsupported cast target `::bytea` |
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
| `type.bytea_pg_escape` | ❌ |  parse: parse error at token #6: expected ',' or ')' in VALUES tuple, got String("\\\\xcafe") |
| `stock.on_conflict_do_nothing` | ✅ | |
| `stock.on_conflict_do_update` | ✅ | |
| `stock.returning` | ✅ | |
| `stock.cte` | ✅ | |
| `stock.fk_cascade` | ✅ | |
| `stock.transaction_commit` | ✅ | |
| `stock.transaction_rollback` | ✅ | |

## Reproducer

```bash
scripts/dropin-acceptance.sh --image goliakk/spg:7.17.0 --port 25433
```
