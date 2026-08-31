# spg → sentori — 7.39.4, and a restart that changed your sort order

**Image:** `goliakk/spg:7.39.4`
**Manifest digest:** `<filled at publish>`
**Battery:** `suite.sh prerelease` — lint, unit, e2e, gates, biz,
dogfood, the release-blocking comparison against PostgreSQL 18.6, the
ironrule wire checks and the three-engine differential — then drop-in
acceptance against the pushed image: **<filled at publish>**.

The comparison against PostgreSQL 18.6 that blocks every release:
**<filled at publish>**.

Four things in this version change ANSWERS rather than messages. One
of them changed every text `ORDER BY` in the database whenever the
server was restarted, and one let a row into a table that should have
been refused. Those two are first below, each with something you can
run against your own data.

This note also covers 7.39.3, which we published and then found to be
half of a fix; it never went out as a note of its own. If you are on
7.39.3, take 7.39.4.

## A restart was changing your sort order

If your database orders text by a locale — which every published image
does, `LANG=en_US.utf8` — then a restart that did not follow a
checkpoint brought it back ordering by BYTES. Same container, same
settings, same table, same query:

```
before restart   a,A,b,B
after restart    A,B,a,b
in the log       spg-server: database collation "C"
```

Nothing was corrupted and no row moved. What changed is the ORDER rows
come back in, and it changed silently: a report run before a bounce and
the same report run after it disagree, with no error on either side.
Every ordinary `kill` and every crash leaves the database in the state
that does this; only a clean checkpoint before shutdown avoided it.

**What to check:** the startup line in your logs. If it says
`database collation "C"` on a server you expect to collate, that
instance has been sorting by bytes since it came up. Anything computed
under it — a paginated list, a report ordered by name, a `<`/`>`
comparison on text — was answered in byte order.

The collation is now recorded next to the WAL and read back before
recovery, so it survives the bounce. The guard that stops a `LANG`
from redeclaring an existing database is unchanged; it was never the
problem, it just had nothing durable to consult.

## A `UNIQUE` column was letting a duplicate in

This is the one to act on, and it is not new in this version — it has
been there for as long as the constraint has been enforced by index
probe. On a database that names a collation, which every published
image does:

```
CREATE TABLE t (id INT NOT NULL, code TEXT NOT NULL UNIQUE);
INSERT INTO t VALUES (1, 'a');
INSERT INTO t VALUES (2, 'a');   -- accepted
SELECT code FROM t;              -- a, a
```

No error, no warning: two rows with the same value in a column declared
UNIQUE. The constraint descends the column's B-tree when that is the
cheaper check; on a collated database that tree is keyed by sort keys
the engine supplies and is empty until it is refreshed, so a lookup by
the raw value answered "nothing here" — which the chooser read as "this
is the most selective candidate", took, and never compared the
conflicting row against. It now declines such a tree and folds the
table, which is the path that ran before the probe existed.

**What to check:** any table with a `UNIQUE` or `PRIMARY KEY` on a TEXT
/ VARCHAR / CHAR column. The duplicates, if any, are in your data now
and this version will not remove them:

```sql
SELECT code, count(*) FROM t GROUP BY code HAVING count(*) > 1;
```

Integer keys were never affected. Nothing that was correctly rejected
became accepted; the fix only closes the hole.

## `SET NAMES … COLLATE utf8mb4_bin` was not reaching your literals

If any of your connections opens with a byte-wise collation — and that
clause is in the first packet of a client that wants one — SPG was
carrying it only as far as trailing-space handling. Whether to compare
case-insensitively was decided by "this is a MySQL session", full stop.
Measured against MySQL 9.7.2 under that exact session:

```
SET NAMES utf8mb4 COLLATE utf8mb4_bin;
SELECT 'AB' = 'ab';     MySQL 0    was 1, now 0
SELECT 'a' < 'B';       MySQL 0    was 1, now 0
```

Both halves are fixed, but they shipped in two versions, and the second
one exists because of how we found it. 7.39.3 fixed EQUALITY, and we
told you it fixed the ordering too. It did not. We found that after
publishing, by running the query against the image we had just pushed
rather than against our own test harness — which was answering
correctly on a database that orders by bytes, while the server ships
collating `en_US.UTF-8`, where the defect lives. **7.39.4 fixes the
ordering.** If you are on 7.39.3, take 7.39.4.

**What to check:** any query whose RESULTS depend on case where the
connection sets a `_bin` or `_cs` collation — equality filters, range
predicates and `ORDER BY` alike. Nothing was corrupted; the stored data
is untouched. What may be wrong is a report's row set or row order,
computed while one of those two versions was serving.

Fixing it exposed the other direction, which we fixed in the same
version: an explicit `COLLATE` on an expression outranks the session,
and our parser had been discarding an explicit `_ci` name in a MySQL
session on the reasoning that the dialect folds anyway. True until the
fold started reading the session. Both directions are pinned now, and
both are in a differential fixture that runs against MySQL 9.7.2 and
MariaDB 12.3.3 on every release.

## Full-text and trigram indexes were answering nothing

On the same collated database, MySQL `MATCH(col) AGAINST ('term')` over
a `FULLTEXT KEY`, and `LIKE '%…%'` over a `gin_trgm_ops` index, both
returned the empty set for rows inserted after the index existed. A GIN
keys on lexemes and trigrams, not on the column's value, so a collation
has nothing to say about it — but the question "does this index take a
key from outside" was answered from the column's TYPE without asking
what kind of index it was, and every GIN over a text column was routed
into the collation machinery.

**What to check:** anything whose results come from a full-text or
trigram search and that has looked thinner than you expected. The rows
are in the table; they were not in the index.

## How the UNIQUE and the index defects were found

Our SQL corpus is 3,889 records and every one of them had only ever run
on a database that orders by BYTES. The image ships with
`LANG=en_US.utf8` and says so on the way up. A defect that needs the
database to name a collation therefore could not appear in that corpus
no matter how many records it grew — which is how the ordering half of
the fix above shipped past nine green gate steps.

The corpus now runs twice, once each way. The second run's FIRST pass
found the UNIQUE hole and the empty index answers, neither of them
about ordering. It is part of the release battery from here.

Two smaller things came out of the same work. Forty-three regression
fixtures — every one written since v7.38.13 — had never run at the
fast gate, because the list that names them enumerated one of two
directories with the same name; the runner now fails when a fixture is
missing from it. And the tracked conformance report had been stale
since v7.39.2.

## Your column names come back as you wrote them

A column created `MyCol` was reported as `mycol` by `SHOW COLUMNS`, by
`information_schema` and by `SHOW CREATE TABLE` — every surface an ORM
reads to diff a schema. The migration such a tool generates from that
reading drops and re-adds a column that never changed. 7.39.2 made the
LOOKUP case-insensitive, which stopped the queries failing; this
version keeps the spelling, which stops the schema diffs.

An unknown `ENGINE=` name is now quoted back as written too
(`Unknown storage engine 'NoSuchEng'`), which matters because the next
thing you do with that message is search your dump for the name in it.

## The messages a MySQL client is given

Four more places where we answered in words MySQL does not use. None of
them changes a result; all of them change what a client or a log parser
can recognise.

- A **wrong argument count** now says what PostgreSQL says
  (`function ltrim(unknown, unknown, unknown) does not exist`) and
  carries MySQL's own errno **1582** on the MySQL wire.
- A **syntax error** now says what MySQL says: `You have an error in
  your SQL syntax; … near '<the rest of the statement>' at line N`.
  This is errno 1064, the error a MySQL user meets most often, and we
  had been sending PostgreSQL's sentence under MySQL's number.
- **`character_sets_dir`** is answered instead of raising
  `Unknown system variable`.
- **`SELECT 'a' 'b'`** names its column `a`, as MySQL does. The value
  was already right.

## What we would like from you

One thing, and it is the `UNIQUE` query above — run it against the
text-keyed tables that matter to you. If it returns rows, tell us and
we will work out with you which of them to keep; we would rather do
that with you than hand you a script.

The rest is not blocking. If you run connections with a `_bin` or `_cs`
collation, the collation section is worth a look at any report whose
row counts you have reason to doubt.
