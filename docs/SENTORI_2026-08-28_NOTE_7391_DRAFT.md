# spg → sentori — 7.39.1, and the value one connection could read from another

**Image:** `goliakk/spg:7.39.1`
**Manifest digest:** `sha256:bebe582e18f913f92a77749c5bb05915c0e175a7b1d9e5b98e4fc9f51a66abdf`
**Battery:** `gate.sh all` — lint, unit, e2e, gates, biz, dogfood — plus
the release-blocking comparison against PostgreSQL 18.6 with both legs
under `C` on a quiet box: **64 cells, no losses, nothing withdrawn**,
worst sort ratio 1.34x against a 3.0x ceiling. Then drop-in acceptance
against the pushed image: **71 of 71**.

Six defects. Two of them are things you would feel.

## One connection could read a value another connection set

This is the one to read first.

`set_config('app.tenant_id', <id>, true)` once per request, then
`current_setting('app.tenant_id')` to scope what the request may see —
that is how row-level security and multi-tenancy are written, on
PostgreSQL and here.

The `SET LOCAL` undo log was **one list for the whole engine**, while
the values it restores live per connection. And the test for "are we
inside a transaction" was true when *any* connection was. Two
consequences, measured with two live sessions and a parameter only
connection B had ever written:

```
A, before anything          <unset>
A, after its OWN commit     'Bvalue'      <- a value only B ever wrote
B, after                    'Blocal'      <- B's LOCAL value outlived its statement
```

So: a request's tenant id could survive into the next request on that
connection, and a connection could come out of its own commit holding an
id set by another. Neither is visible without concurrency — which
production has and a test bench does not.

Both halves are fixed. The undo log and the savepoint marks are keyed by
transaction now, and the three places that asked "are we in a
transaction" separately go through one place that asks about *this*
connection. There are six pins on it, run with two sessions and two
transaction slots, because the 6,600 tests in the engine harness run one
session on one slot — where neither half of the defect exists.

## Ordinary MySQL SQL failed on an error naming a column nobody wrote

```sql
SELECT "abc"
```

MySQL 9.7.2 answers `abc`. We answered `ERROR 1054 column "abc" does not
exist`. We were treating `"…"` as an identifier — PostgreSQL's rule,
applied to a MySQL session — and a great deal of MySQL SQL quotes
strings that way.

Every rule was read off MySQL 9.7.2 before anything was written:
`"a""b"` and `"a\"b"` are both `a"b`, `"a'b"` is `a'b`,
`LENGTH("\n")` is 1 unless the session says `NO_BACKSLASH_ESCAPES`,
backticks are unaffected, and `ANSI_QUOTES` turns the identifier rule
back on for `"` alone. That last one was not modelled at all before:
`SET sql_mode='ANSI_QUOTES'` succeeded and changed nothing.

Your PostgreSQL sessions are untouched — `"…"` is an identifier there
whatever a MySQL `sql_mode` said, and the pins check that direction too.

## The other four

- **A value that will not fit is refused the way MySQL refuses it.**
  `1264 (22003) Out of range value for column 'x' at row 1` and
  `1406 (22001) Data too long for column 'x' at row 1`, byte for byte.
  The refusal was already correct; the *code* was PostgreSQL's, and a
  driver that branches on the code branched wrong.
- **One question about a parameter name, one answer.** `SET`, `SHOW`,
  `current_setting` and `set_config` gave four different answers to
  "is `nosuch_guc` a parameter?" — and two of them reported it as read
  or applied when neither happened, which is how a typo goes unnoticed.
- **A reflection can read a column's charset and collation.**
  `CHARACTER_SET_NAME` errored as a missing column and `COLLATION_NAME`
  answered NULL for every column without an explicit `COLLATE`, so an
  ORM could not tell a case-insensitive column from a binary one.
- **`@@collation_database` and `@@character_set_database`** answered
  `Unknown system variable`; the `SHOW VARIABLES` inventory listed one
  of MySQL's three collation names and one of its eight character-set
  names. Under an identical handshake they now match 9.7.2 exactly.

## What we found and did NOT fix

Written down because a gap named is not the same thing as a gap claimed
closed.

**An unquoted identifier is folded to lower case here and a backticked
one is not — so the two spellings name different tables.** Measured, in
one session:

```sql
CREATE TABLE MyTable (MyCol int);   -- stored as mytable
SELECT 1 FROM `MyTable`;            -- ERROR 1146: relation does not exist
```

`mysqldump` backticks every identifier. If you restore a MySQL dump here
and your application writes the name unquoted, those are two tables.
MySQL 9.7.2 keeps `MyTable`, reports it that way from `SHOW TABLES` and
`information_schema`, and finds it under either spelling; meanwhile
`@@lower_case_table_names` answers `0` on both sides, which from us is a
claim we do not keep.

This is identifier handling in the parser, not a reporting surface, and
changing it moves where existing data lives — so it did not go into
7.39.1.

**It is fixed now and will be in the next release.** A MySQL session
compares relation names without case, which is MySQL's
`lower_case_table_names = 1` — and that is what we report now, instead
of the `0` above, which asserted a case-sensitivity we never performed.
A table already created with its case kept keeps that name and becomes
reachable under any spelling, rather than only its own: we did not fold
at CREATE, because that would have made exactly those tables
unreachable. Your PostgreSQL sessions are untouched, and pinned in both
directions.

**If any of your schema uses mixed-case names, tell us** — we would
rather hear it before that release than after.

Also open, and smaller: a MySQL session's unknown-column error says
`column "x" does not exist` where MySQL says `Unknown column 'x' in
'field list'` (the code 1054 and SQLSTATE 42S22 already match); a
duplicate column name in `CREATE TABLE` is accepted here and refused by
both PostgreSQL and MySQL; and `SHOW VARIABLES` still omits three
character-set entries, one of which names a directory we do not have.
