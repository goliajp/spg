# MySQL-dialect differential corpus — SPG's mysql-wire vs live MySQL 9.7

The PostgreSQL half of this idea is `xtests/diffcorpus/`. This is the
MySQL half, and it did not exist until v7.40.0.

## Why it exists

SPG advertises a MySQL face: a `mysql-wire` listener, `SELECT VERSION()`
answering `9.7.2-spg`, MySQL's own error numbers and SQLSTATEs. The only
systematic check on that face was `xtests/diffcorpus/mysql-19.sql` —
fifteen statements, run against **MariaDB**.

MariaDB is a different engine. It is not the one SPG claims to be, and
where the two disagree the corpus was grading SPG against the wrong
answer. v7.38.16 already paid for this once: SPG's trailing-space
comparison rules had been calibrated to MariaDB's `PAD SPACE` while SPG
reports itself as MySQL 8.0, whose default collation does not pad.

So: thirteen files, in MySQL's own spelling, against `mysql:9`.

`xtests/diffcorpus/mysql-19.sql` and its MariaDB runner stay where they
are. SPG is a drop-in for MariaDB too, and that file is the only thing
measuring it; this corpus does not replace it, it covers the engine SPG
actually reports itself to be.

## Protocol

Identical in shape to the PostgreSQL corpus, and for the same reasons —
each rule below was bought:

* **Start our own server.** Reusing whatever is on the port grades an
  unknown build. `SPG_REUSE=1` opts back in for hand-driving.
* **Prove both legs answer `SELECT 1` before scoring.** Two legs that
  fail the same way diff to zero and print IDENTICAL, which reads as a
  sweeping improvement. The PostgreSQL corpus reported all twenty
  categories identical exactly once, with its oracle container down.
* **One client binary for both legs.** A rendering difference must be
  the engine's. Both legs run the oracle container's `mysql` client.
* **`--force`.** Without it the client stops at the first error, and one
  missing function early in a file deletes every answer after it. The
  first run scored file 01 at fifteen differing lines, of which exactly
  one was real; the rest was truncation.
* **`--default-character-set=utf8mb4` on both legs.** The first run
  reported `CHAR_LENGTH('日本')` as 6 on the oracle and 2 on SPG — a
  three-line finding in file 03 that was the connection charset, not
  either engine. Naming it removed the variable and file 03 went to
  IDENTICAL.
* **The two streams are compared separately.** `mysql` writes rows to
  stdout and errors to stderr, and which lands first is a race between
  two descriptors on one pipe: measured, the same run put a
  duplicate-key ERROR before its own marker row on one leg and after it
  on the other, scoring two differing lines that were the same two
  lines. The PostgreSQL corpus separated its streams in round 666 for
  exactly this.
* **Baseline, not zero.** Every run differs; the gate fails on
  DEVIATION from `baseline.tsv`. `--rebaseline` is a deliberate act and
  belongs in a commit message with its reason.
* **Prove which BINARY answered.** `rsync -a` preserves mtime, so cargo
  can decide a source file is not newer than the artefact and skip the
  rebuild — and the corpus then grades a build from before the change.
  It happened once: two files that had been IDENTICAL for three runs
  came back with nine and five differing lines, every one of them an
  answer the previous build gave, and the numbers were recorded as a
  baseline before the cause was known. The SPG leg now says its
  `VERSION()` and it has to be the workspace's.
* **Name the oracle.** `mysql:9` is a rolling tag that moves under a
  running project. A verdict names both of its sides.

## Two things the harness cannot remove

* The oracle must be **in** a database and SPG's wire serves one
  built-in schema, so the two legs are in differently-named databases.
  The name reaches the reader inside error messages; `norm` rewrites
  both to `<db>.`.
* The oracle requires TLS (MySQL 9's `caching_sha2_password` refuses a
  plaintext password) and SPG's wire has no password at all, so the SPG
  leg runs `--ssl-mode=DISABLED`. That asymmetry is in the transport,
  not in any answer.

## Running

```
bash xtests/mysqlcorpus/run.sh                 # all files, gate on baseline
bash xtests/mysqlcorpus/run.sh 05-*.sql        # one file
bash xtests/mysqlcorpus/run.sh --rebaseline    # record the current state
```

Output lands in `out/` (gitignored): `<name>.spg`, `<name>.mysql`,
`<name>.diff`.

## Classifying a difference

Same three buckets as the PostgreSQL corpus. Every differing line is
one of:

* **NEW-DEFECT** — a silently wrong answer, a missing error, a spurious
  error, or a PostgreSQL sentence emitted on the MySQL wire. Opens a
  correctness entry unconditionally.
* **KNOWN** — already ledgered.
* **NEW-DIVERGENCE** — judged deliberate (SPG stricter, or an
  environment difference). Goes in the ledger **with the measurement
  that supports it**.

## The first baseline (v7.40.0, MySQL 9.7.2)

The first run scored **85** differing lines across thirteen files. The
recorded baseline is **2**, and twelve of the thirteen files are
IDENTICAL.

Those two lines are the one difference this version records as a
DECISION rather than a defect: `->` and `->>` accept a literal left
operand where MySQL raises a syntax error, so SPG is the more permissive
of the two. Everything else the corpus found is fixed. `FINDINGS.md`
beside this file has every line, with the measurement behind it.

Three consecutive runs of the gate exit 0.
