# spg → sentori — 7.39.0, and what we were telling you that was not true

**Image:** `goliakk/spg:7.39.0`
**Manifest digest:** `sha256:2fd50017fb945f20cb7543d0f647ecab420483e1ee6644d74f9c30c07eaad52d`
**Battery:** the release train's own, and it stopped us nine times before
it let anything out. `gate.sh all` — lint, unit, e2e, gates, biz,
dogfood — plus a release-blocking performance comparison against
PostgreSQL 18.6 with both legs under `C` on an idle box: **64 cells, no
losses**, the worst sort ratio 1.23x against a 3.0x ceiling, and several
cells ahead. Then drop-in acceptance against the pushed image:
**71 of 71**.

Every defect in this release is the same shape, and it is the worst
shape a database can have: **we told your session it held a guarantee
that nothing was keeping.**

Your application asks what isolation level it is running under, whether
its transaction is read-only, whether the session is strict. It asks for
exactly one reason — to decide what it may assume. An answer that is
wrong in the cautious direction costs a little performance. An answer
that is wrong in the other direction is worse than no answer at all,
because your code acts on it.

Two of these lost data. One of them lost it without an error.

## The two that lose data

**A fresh MySQL connection was not strict.** `VARCHAR(3)` given
`'abcdef'` stored `'abc'`. `TINYINT` given `999` stored `127`. No error,
no warning, a success response — and `@@sql_mode` told that same
connection `STRICT_TRANS_TABLES` throughout. MySQL 9.7.2 answers
`ERROR 1406` and `ERROR 1264` for both.

The cause was a default: a connection's session state is created on
first sight from a derived `Default`, where a boolean is `false`, and
the strictness flag's correct value is `true`. Its own documentation had
said "this starts true" since the flag was added — which was true of the
engine's constructors and never of the per-connection state.

If you have written into a MySQL-wire column narrower than the value you
sent, on any release before this one, the value was silently cut. We
cannot tell from here whether that happened to you; the check is
whether any `VARCHAR(n)` / `CHAR(n)` column holds values of exactly
length `n` that should have been longer.

**Read-only transactions were not enforced at all.** `BEGIN READ ONLY;
INSERT …` answered `INSERT 0 1` and committed.
`SET default_transaction_read_only = on` changed nothing either, while
reading back whatever you set. Nothing in the engine enforced either
one — no classifier for whether a statement writes, no SQLSTATE 25006
anywhere.

If you use a read-only transaction or a read-only pool leg as a
guardrail — "this code path must not write" — that guardrail was not
there. Every statement PostgreSQL 18.6 refuses is refused now, with
PostgreSQL's message and code.

## The rest of the same shape

- `SET default_transaction_isolation` did nothing. We accepted it, read
  it back correctly, and ran read committed anyway. If your pool sets it
  at connect time, you were told you had repeatable read and did not.
- `transaction_isolation` was answered by four different places and two
  of them contradicted the engine.
- A bare `SET TRANSACTION ISOLATION LEVEL`, outside a transaction block,
  changed the session — so one of them quietly changed every later
  transaction. PostgreSQL warns and does nothing.
- `SHOW VARIABLES` reported the compiled-in default for every variable,
  ignoring anything you had `SET` — including `SET NAMES`.
- `SET NAMES … COLLATE` was parsed and discarded, so a session compared
  strings by a collation it did not report.
- `ENGINE=` was never checked, so a typo in a dump quietly became a
  table while `sql_mode` claimed `NO_ENGINE_SUBSTITUTION`.

## What we are NOT claiming

Written down because a gap on paper is not the same thing as a gap we
have told you is closed:

- `ONLY_FULL_GROUP_BY` is not enforced. We never claimed it either, and
  `sql_mode` is now pinned so it cannot appear until it is true.
- `NO_ENGINE_SUBSTITUTION` is claimed again, but only for the half we
  can honour: a name MySQL rejects is rejected. We have one storage
  engine and substitute for every name we accept, `ENGINE=MyISAM`
  included.
- An unquoted unknown engine name is echoed lower-cased in the error —
  `'nonsuch'` where MySQL says `'NONSUCH'`. The code, the SQLSTATE and
  the refusal all match; the quoted forms match exactly.
- Over-length and out-of-range values are refused, but the wording is
  PostgreSQL's, and the overflow code is `1064/42000` where MySQL uses
  `1264/22003`. If you branch on the code, that branch is still wrong.

## How this was verified

Expectations came from running the same statements against PostgreSQL
18.6 and MySQL 9.7.2 rather than from documentation — one statement per
transaction, so no error could be attributed to a neighbouring line.
Five answers were not what the documentation suggests, including that
`CREATE TEMP TABLE` is refused in a read-only transaction and that
`UPDATE … WHERE false`, which changes nothing, is refused too.

The isolation corpus was generated by pointing our runner at PostgreSQL
rather than blessing our own output. That same run re-generated the
seven specs we already had, and they came back byte-identical.

Every fix was ablated — reverted, re-run, and required to fail — with
the server binary rebuilt each time and its checksum printed, because
two ablations in this cycle scored perfect neutral results off a stale
build before we noticed.

## Performance

64 cells against PostgreSQL 18.6, both legs collating identically under
`C`, on a box with a load average of 1.6 — **no losses**. Several cells
are ahead: an indexed numeric key, a bytea key, top-N and equality
probes. The sort panel's worst ratio is 1.23x against a ceiling of 3.0x.

Two things we will not dress up. The gate refused to score four earlier
attempts, and it was right every time: twice because the two legs were
collating differently — which can turn a 3x loss into an apparent 2x win
on the same query, so the comparison would have been meaningless — once
because a leg was not up, and once because nothing had been compared at
all and it treats that as a failure rather than a skip on a release run.
We did not use the escape hatches that exist for exactly those cases.

And there is a panel this release does NOT judge, which we mention
because its numbers are visible in our logs: the locale panel compares
SPG-under-a-declared-collation against SPG-under-C, and it moves between
runs on unchanged code — we measured 0, 0 and 2 losses from the SAME
build. It is reported, not gated, and we spent an hour and a half
proving to ourselves that a difference we thought we saw there was not
real before shipping.
