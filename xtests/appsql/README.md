# appsql — ask a real application's statements, on both backends

A drop-in claim is about somebody's application, not about a corpus we
wrote. This harness takes the SQL that application actually contains,
puts the same schema on SPG and on PostgreSQL 18, and Describes every
statement on both — then diffs the answers.

Describe is where a drop-in fails quietly. It needs no data and no
fixtures: the question "what columns and types does this statement
return" is answerable from the schema alone, so an entire application's
surface can be checked in seconds, including the pages its own test
suite has not reached yet.

## Why it exists

sentori's suite advanced a few steps per release, hitting one Describe
defect each time — a data-modifying CTE at step 16, a subquery in the
select list at step 30, a top-level `IS NOT NULL` at step 41. Each cost
them a round trip and us a release. Running their 211 statements
directly found thirteen more divergences in one pass, four distinct
causes, all on pages they had not reached.

## Use

    # 1. extract the SQL literals from the application's sources
    python3 extract-sql.py > /tmp/app-sql.json     # edit ROOT inside

    # 2. put the application's schema on both backends, then
    cd differ && cargo run --release
    # env: SQLJSON, SPG, PG

Output is one block per divergence and a one-line verdict. `SPG-ERR`
means we refused a statement PostgreSQL accepted — always a defect.
`DIFF` means both answered and disagreed. `PG-ERR(ours ok)` means we
accepted something PostgreSQL rejects, which is usually us being too
permissive.

## The two traps this harness has already fallen into

Both are recorded because they produce output that looks exactly like a
finding:

- **Pointing at the wrong backend.** The schema was applied to one
  PostgreSQL container and the differ pointed at another; it reported
  210 "PG errors", every one of them `relation does not exist`.
- **A setup step whose failure was discarded.** The migrations ran with
  output redirected to /dev/null against a server that was not up yet;
  the differ then reported that SPG described nothing for 124
  statements.

Verify the table count on BOTH sides before believing any verdict. The
differ prints nothing about its own setup, so the setup has to prove
itself.
