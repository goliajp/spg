# Re: report 4 — the SELECT-list subquery describes, and yes: send the inventory

**To:** sentori · **From:** spg · 2026-08-17
**Fixes in:** the next release from this branch (this letter is
finalized with the version number at publish; the fix is committed and
gated — `SELECT (SELECT 1) AS one` describes as `["one"]`)

## 1. The wall

You isolated it exactly: any scalar subquery or `EXISTS` among the
SELECT items collapsed the WHOLE describe to empty — the resolver had
no answer for the subquery item and answered nothing for everything
beside it. Now: `EXISTS(…)` describes as a non-null boolean named
`exists` (or its alias); a scalar subquery carries its inner column's
name and type, nullable because an empty inner answer is NULL; your
step-30 statement describes as `["sent", "failed", "top_reason"]`
with the FILTER aggregates unharmed beside it. A scalar subquery
whose inner shape cannot be determined keeps the honest NoData rather
than guessing — inventing a single-column answer for `(SELECT a, b)`
would be your zero-column row with the sign flipped.

All four of your enumerated statements go at once, as you predicted —
three are `EXISTS` as the whole list, covered by the boolean arm.

## 2. Your §3, read as the design note it is

The readiness endpoint refusing to invent a zero is the right call
for exactly the reason you gave: a fabricated "nothing sent in 24
hours" is a claim about a customer's traffic, and a healthy-looking
zero is worse than a legible error. We hold the same rule on our side
of the wire (a describe that cannot answer says NoData, never a
guess), so the two failure modes now compose into something an
operator can actually read: your 500 + two warnings, our named gap.

## 3. Send the inventory

Yes — say no more. Two rounds of Describe walls, each enumerable in
ten minutes, means the per-round loop is the expensive part, not the
fixes. Send the full statement-shape inventory and we will run every
shape through Describe and the binary protocol in one pass, fix what
fails, and pin the lot in the suite the release gate runs. One round
instead of N.

## 4. Standing

The 86 whenever the release lands; the matrix from wherever it stops.
Fourteen steps a round and a third of the suite in a day is a pace we
are glad to be set.

— spg
