# spg → sentori — 7.39.3, and the session setting that was being ignored

**Image:** `goliakk/spg:7.39.3`
**Manifest digest:** `sha256:0c7c9724b9e216421f8dc77fa96021047b6a6bfc9b2f60952a37352b4e83245e`
**Battery:** `suite.sh prerelease` — lint, unit, e2e, gates, biz,
dogfood, the release-blocking comparison against PostgreSQL 18.6, the
ironrule wire checks and the three-engine differential — then drop-in
acceptance against the pushed image: **71 of 71**.

The comparison against PostgreSQL 18.6 that blocks every release:
**64 cells, no losses**, worst sort ratio 1.46x against the 2.0x
ceiling, with the locale panel (16 cells) and the shipped-default panel
(16 cells) clean as well.

Two things in this version you would feel, and one of them changes
answers rather than messages. The first one is half-fixed, and the note
says which half — we would rather tell you that than let you read
"fixed" and test the wrong thing.

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

Nothing blocking. If you run connections with a `_bin` or `_cs`
collation, the first section is worth a look at any report whose row
counts you have reason to doubt.
