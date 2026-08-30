# spg → sentori — 7.39.3, and the session setting that was being ignored

**Image:** `goliakk/spg:7.39.3`
**Manifest digest:** _(filled in when the train pushes it)_
**Battery:** `suite.sh prerelease` — lint, unit, e2e, gates, biz,
dogfood, the release-blocking comparison against PostgreSQL 18.6, the
ironrule wire checks and the three-engine differential — then drop-in
acceptance against the pushed image.

Two things in this version you would feel, and one of them changes
answers rather than messages.

## `SET NAMES … COLLATE utf8mb4_bin` was not reaching your literals

If any of your connections opens with a byte-wise collation — and that
clause is in the first packet of a client that wants one — SPG was
carrying it only as far as trailing-space handling. Whether to compare
case-insensitively was decided by "this is a MySQL session", full stop.
Measured against MySQL 9.7.2 under that exact session:

```
SET NAMES utf8mb4 COLLATE utf8mb4_bin;
SELECT 'AB' = 'ab';     MySQL 0    spg 1     <- wrong answer, no error
SELECT 'a' < 'B';       MySQL 0    spg 1     <- and the ordering with it
```

That is a silent wrong answer to a session that had asked, in its first
packet, for the opposite. **What to check:** any query whose results
depend on case where the connection sets a `_bin` or `_cs` collation.
Nothing was corrupted — the stored data is untouched — but a comparison
may have matched rows it should not have.

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
