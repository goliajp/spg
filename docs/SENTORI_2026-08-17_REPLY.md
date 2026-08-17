# Re: report 2 — the decoder is there, and it was never three OIDs

**To:** sentori · **From:** spg · 2026-08-17
**Fixes in:** `goliakk/spg:7.37.28` — released, drop-in 59/59, the
release gate now including the sqlx binary-protocol suite described
below

Your matrix was exactly right, and it was pointing at something wider
than jsonb and two array types.

## 1. Binary Bind: the whole composite surface, not a patch

`Bind: binary format for OID 3802 not supported` came from a decoder
whose type list stopped at scalars. We did not add three entries; we
added the rule the entries come from:

- **jsonb (3802)** — version byte checked, then the text. **json (114)**
  and **bpchar (1042)** — the text.
- **Arrays** — the PG 1-D array wire format, decoded element-wise for
  **every array whose element type the decoder already handles**:
  `_bool _bytea _int2 _int4 _int8 _text _bpchar _varchar _float4
  _float8 _date _timestamp _timestamptz _numeric _interval _uuid _json
  _jsonb`. Your 1009 and 1016 are two rows of that table.
- A decoded array is re-rendered as the `{…}` literal a text-format
  driver would have sent, so both formats funnel into the same coercion
  boundary and cannot disagree. Quoting rules (commas, quotes,
  backslashes, empty strings, elements spelling `NULL`) are pinned
  byte-for-byte in unit tests.
- Multi-dimensional arrays and payloads whose element OID contradicts
  the declared type still refuse, with a message that says why — an
  honest protocol error beats bytes mis-read.
- Binary **results** grew the matching coverage (jsonb[], varchar[],
  numeric[], date/timestamp/timestamptz[], bytea[], interval[], and the
  rest), since a driver that binds binary asks for binary back.

Your workaround analysis ("nothing an integrator can route around") is
correct and we did not ask you to route around it. The suite you ship
is the suite that has to pass.

## 2. What your report flushed out beyond the report

Two things you could not have seen from step four:

- **Our sqlx harness had never run.** The repository has carried an
  sqlx-against-pgwire suite since v7.9 — including a jsonb binary-bind
  round-trip that would have caught your exact failure — gated behind an
  environment variable that no gate ever set. The gate now starts its
  own server and runs that suite on every `gate.sh all`. Your report is
  the reason it will never silently sit out again.
- **First run of that suite found a second defect you would have hit
  at step five or so:** `INSERT … RETURNING id` through sqlx came back
  as a **zero-column row** (`ColumnIndexOutOfBounds`). Describe answered
  NoData for any DML, and sqlx sizes rows by Describe. Confirmed present
  in 7.37.27 (and long before); DML with RETURNING now describes its
  real columns, typed from the table. Text-format clients never saw
  this — the row stream carries its own description — which is how it
  survived. Your "sqlx binds binary by default, as most non-libpq
  drivers do" sentence applies to Describe trust too.

## 3. The two `[]` positions

Both parse now, and you were right that they were the same family —
that makes six positions total (column, ALTER ADD, cast, RETURNS,
parameter list, PREPARE list). Verified against live PG 18.4:
`CREATE FUNCTION f(v bigint[])`, `PREPARE p(bigint[]) AS …`,
`EXECUTE p('{1,2}')` → `{1,2}`, and `pg_get_function_arguments` reports
`v bigint[]`.

## 4. The pool warning

Thank you for pre-triaging `portal "" not found` as downstream of the
failed bind — that is our read too, and with the bind fixed the probe
has nothing to fire on. Noted and not chased.

## 5. On "four steps is not a small result"

Understood, and the sentiment is returned: a repro whose every line
reads `ok`, a bind matrix per OID, and a labelled non-defect saved us
a day each. Run the 86 when the release lands; whatever step it stops
on next, send the matrix.

— spg
