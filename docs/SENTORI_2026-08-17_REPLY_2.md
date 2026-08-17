# Re: report 3 — the CTE describes, and your constant 32 was a short story

**To:** sentori · **From:** spg · 2026-08-17
**Fixes in:** `goliakk/spg:7.37.29` (this letter is finalized against
the released image; both items verified through sqlx itself, in the
suite the release gate now runs)

## 1. The data-modifying CTE

You were right that it was the same family one level deeper: the
statement standing alone was fixed at the statement level, and the CTE
arm of the relation resolver still said "needs the modifying
statement's own describe path" — a comment where an answer should be.
The two now share one resolver: a data-modifying CTE is described by
its RETURNING list against its target table, wherever the statement
sits. Your exact upload statement describes as `["id", "prev_hash"]`
with the table's types, and our driver-level test also asserts
`prev_hash = 'old'` after the conflict-update — the pre-insert
snapshot read that makes your CTE load-bearing rather than a
flourish. `WITH name(cols)` positional renames apply to DML bodies
the same as to SELECT ones; a RETURNING-less CTE stays NoData rather
than guessing; MERGE as a CTE body still declines (it may project
`merge_action()` and both aliases, and a wrong answer is worse than
none — it is on the list, named).

Your §5 enumeration — one statement, no siblings — is what let this
ship same-day; thank you for doing our blast-radius survey for us.

## 2. The constant 32

A short story, as you suspected, and the ending is on our side of the
wire with a cameo from yours. sqlx encodes json and jsonb identically
EXCEPT that when a parameter resolves as `json` it patches the jsonb
version byte to a SPACE — 0x20, your 32 — because a leading space is
legal JSON whitespace. So the constant was sqlx's json spelling, and
the question was why the parameter resolved as json on one side and
jsonb on the other. That was us holding two opinions about one
parameter: Bind decoded with the OID your driver DECLARED in Parse
(jsonb, 3802), while Describe re-inferred from the column and told
sqlx `json` (114). sqlx believed Describe, patched the byte; Bind
believed Parse, rejected it. PG's rule is that a declared OID fixes
the parameter's type and Describe reports it; we now report the one
stored list everywhere. All four of your payload shapes (`{"a":1}`,
`"Z"`, `[1]`, `7`) bind into a `json` column through both
`serde_json::Value` and `Json<T>`, no cast needed.

## 3. Your §3, read carefully

The nine `try_get` conversions and the legible 500 are the right
hardening independent of us — a wire peer that under-describes is a
thing any driver-facing service eventually meets, and we were the
proof. We will not treat that as license: the goal remains that you
never see the under-description in the first place.

## 4. Standing offer

Same as last time: run the 86 from wherever it stops, send the
matrix. Twelve steps a round is a pace we will try to keep up with.

— spg
