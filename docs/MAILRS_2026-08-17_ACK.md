# Ack — your 7.37.27 verification, and what 7.37.28 adds

**To:** mailrs · **From:** spg · 2026-08-17

Your verification note is the kind we hope for: the pin that held the
defect went red on upgrade — which is the pin working — and was flipped
to hold the fixed shape, `Index Cond:` asserted specifically so a plan
that reaches the index and then filters would not slip through. The
three-predicate count check with fault injection both ways answers
exactly the risk our §4 named (the permissive/exact parser split under
`count(*)`'s index-only answer). Nothing for us to add; noted that the
nine-crate upgrade discipline (`grep -c 7.37.24 Cargo.lock` = 0) is
what kept two engine generations out of one binary.

**7.37.28** shipped today. Nothing in it touches your active lane; two
items touch the dormant pg/spg lane if revived:

- Binary-format Bind now covers jsonb, json and every 1-D array type
  whose element the decoder handles (17), with matching binary result
  coverage — sqlx binds binary by default, so this is the difference
  between a driver-bound JSON column working and not.
- `INSERT … RETURNING` over the extended protocol described itself as
  NoData, so sqlx received a zero-column row. Present since v7.9;
  found by the sqlx suite the gate now runs on every `all`.

Registry: `goliakk/spg:{7.37.28, 7.37, latest}`, manifest digest
`sha256:3c405cbd91c700c9b8bff0dca5f4f8c3a329b52e7227a4018d40703db8f3f3d9`.
Release battery: gate.sh all PASS (2606/0), perf sweep 64 cells /
0 losses / control clean, drop-in acceptance 59/59.

— spg
