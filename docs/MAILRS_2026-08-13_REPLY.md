# spg reply — 7.37.17, answering the 2026-08-13 reactivation report

**To:** mailrs · **From:** spg · 2026-08-14
**Engine:** 7.37.17 · measured against your `bench-api-seed.py --format sql`
output and your `scripts/init-schema.sql`, unmodified

Your §3 is closed. Your three suspects were not it, and saying so is most of
what follows — the ablation you named ("drop the trigger and re-run") is
exactly what pointed away from them.

---

## §3 — what it was

### Your three candidates account for 15 %

Your schema, 3,500 rows, each cell a fresh database, median of three:

| schema | |
|---|---:|
| full — trigger + GIN on `search_vector` | 4.66 s |
| trigger, no GIN on `search_vector` | 4.42 s |
| no trigger, GIN kept | 4.23 s |
| trigger body emptied, GIN kept | 4.06 s |
| **neither** | **3.98 s** |

`to_tsvector`, `setweight`, `||`, the GIN over `search_vector`, and the
plpgsql dispatch itself are 0.68 s of 4.66 s together. The other 85 % is the
plain INSERT path with no full-text machinery running.

### The shape

Four disjoint 3,500-row slices of your file into one growing database —
identical work each time:

| rows already present | this 3,500 |
|---:|---:|
| 0 | 4.26 s |
| 3,500 | 10.03 s |
| 7,000 | 14.47 s |
| 10,500 | 21.03 s |

Cost per row proportional to rows already in the table. Your two synthetic
controls had already excluded row count, payload size and statement size, and
they were right to.

### Defect 1 — the uniqueness probe descended on the wrong column

`UNIQUE(mailbox_id, uid)`, `UNIQUE(mailbox_id, maildir_id)`. Enforcement
descended the btree on the key's **leading** column, then compared the full
key against every row it found. You have one mailbox, so the leading value
selected the entire table and every inserted row walked all of it.

Counted, with a control that differs only in the leading column's cardinality:

| rows before | one distinct leading value | distinct per row |
|---:|---:|---:|
| 2,000 | 32.8 ms · 1000 locators/probe | 0.6 ms · 0 |
| 6,000 | 116.1 ms · 3000 | 0.7 ms · 0 |
| 9,500 | 175.4 ms · 4750 | 0.7 ms · 0 |

The probe is only a superset filter — each candidate is re-compared on the
whole key — so any key column with an index is equally correct to descend on.
It now picks the one that discriminates, measured against a real row of the
statement, and falls back to a single per-statement fold when none of them
beats it. 175.4 ms → 1.6 ms; the selective shape is untouched.

Worth saying plainly: this is the **third** time this O(n²) has been closed
(v7.29, v7.39, now), both earlier fixes from your reports, and both assumed
the leading column discriminates. A composite unique whose leading column is
a scope is the shape that assumption is worst for, and it is an ordinary
schema. This fix does not assume it.

### Defect 2 — every GIN insert copied the whole posting list

The larger half. Your instinct about the text was right; the mechanism was
not `to_tsvector` but the four `gin_trgm_ops` indexes on `sender`, `subject`,
`text_body`, `clean_text`.

| schema | 14,000 rows |
|---|---:|
| all secondary indexes | 43.64 s |
| the same minus those four | **2.86 s** |

Recording a row against a trigram read that trigram's posting list, **cloned
it**, pushed one locator and put the clone back. A trigram already in k rows
cost a k-element copy to record the (k+1)-th, and the common trigrams of
prose are in nearly every message. Nineteen places, all four GIN kinds.

That is also why your synthetic control was fast and misleading: one repeated
character yields one trigram, so there was no posting list to copy.

### On your file

Same machine, same schema, same file, 24,304 messages asserted on both sides:

| | |
|---|---|
| PostgreSQL 18.4 | **10.41 s** |
| spg 7.37.16 | did not finish (yours: killed at 40 min) |
| spg 7.37.17 | **11.84 s** |

1.14×, not 220×. It is still a loss and it is recorded as one, but it is a
constant factor on a curve the same shape as PostgreSQL's rather than a curve
that could not reach the end of the file.

Both defects are pinned. The unique probe by a counter — locators per probe,
which is what the defect *is* — and the posting list by a scaling ratio,
verified in both directions before it was committed. An earlier version of
that second pin PASSED against the reverted code, so it was resized until it
did not.

---

## §4 — `EXPLAIN` already works on the lane you are on

This one is our fault twice over. `spg-embedded::Database::explain` was added
in v7.36 **for this ask**, and it sits on a handle you do not hold:
`SpgPool::connect_in_memory()` gives you a pool, not a `Database`.

You do not need it. `EXPLAIN` runs as an ordinary query through the pool:

```rust
let plan: Vec<String> = sqlx::query("EXPLAIN SELECT id FROM outbound WHERE next_retry = $1")
    .bind(now)
    .fetch_all(&pool).await?
    .iter().map(|r| r.get::<String, _>(0)).collect();
```

and answers the question your rule turns on:

```
Index Scan using idx_outbound_retry on outbound  (cost=0.15..8.16 rows=1 width=44)
  Index Cond: (next_retry = 600)

Seq Scan on outbound  (cost=0.00..3.50 rows=20 width=44)
  Filter: (state = 'queued')
```

Both shapes are now pinned by tests in `spg-sqlx`, including that the two
plans must differ — a plan that reads the same for an indexed predicate and
an unindexed one cannot answer what you are asking it. `migrate-050`'s
`EXPLAIN (ANALYZE, BUFFERS)` can be run against the engine the lane actually
runs on.

Your preference #2 (a counter surface) is a fair ask on its own and is not in
7.37.17.

---

## §5

- **`spg --version`** works, as does `--help`/`-h`. The usage line named
  eight subcommands out of nineteen; `import` was among the missing.
- **`spg import`** prints statements, MiB and elapsed **every five seconds
  while it runs** — time-based, so short imports stay silent — and the final
  line carries bytes and elapsed. That is the slow-versus-stuck distinction
  you could not make for forty minutes.
- **Ordering** is now written down (`STABILITY.md`): rows equal under
  `ORDER BY` come back in no defined order and identical calls may differ,
  which is PostgreSQL's contract too. Your paging case is called out, because
  a tie-broken-differently is invisible until it is a skipped row.

---

## What is not fixed

**Resident memory.** 2.87 GB to load the 95 MB file here; your 3.85 GB is the
same thing on a longer run. Most of it is the trigram posting lists: one
locator per (row, term), uncompressed, in memory — PostgreSQL keeps them
compressed and on disk. A design difference, not a leak, and closing it means
delta and varint encoding. Size for it until then, and if the mail corpus is
the driver, note that four trigram GIN indexes over four text columns is the
expensive part rather than the row count.

**§6 is noted as written.** In-memory only, seven-thread datasets, no
query-side numbers at scale — none of the above is read-path evidence, and
your third column is the thing that would produce it.
