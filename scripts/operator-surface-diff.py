#!/usr/bin/env python3
"""v7.39 (round 508) — which of PG18's operators does SPG not have?

r507 found SPG had no unary `+` at all, and nothing caught it, because the
operator surface had never been swept. This sweeps it generatively: the list
comes from PG's own `pg_operator`, not from memory.

Each operator is exercised with NULL operands of its declared types, e.g.
`SELECT NULL::int4 # NULL::int4`. That asks one question — does the server
RESOLVE this operator — without dragging in value-domain errors. PG answers
NULL; a server that lacks the operator says so instead.

Both servers are asked in ONE psql session each, with a marker line before
every statement, so the whole sweep costs two connections rather than two
per operator.

  operator-surface-diff.py <pg_host> <pg_port> <spg_host> <spg_port>
"""

import subprocess
import sys

TYPES = (
    "int2 int4 int8 numeric float4 float8 text bool date timestamp timestamptz "
    "interval jsonb json inet uuid bytea time money bit varbit tsvector tsquery "
    "point box circle lseg line path polygon macaddr cidr xml"
).split()

MARK = "@@CASE"


def url(host, port, db, user):
    return f"postgres://{user}@{host}:{port}/{db}?sslmode=disable"


def psql_batch(u, script, timeout=900, password=None):
    env = ["-e", f"PGPASSWORD={password}"] if password else []
    r = subprocess.run(
        ["docker", "run", "--rm", "-i", *env, "postgres:18-alpine", "psql", u, "-A", "-t"],
        input=script, capture_output=True, text=True, timeout=timeout,
    )
    return r.stdout or "", r.stderr or ""


def list_operators(u, password=None):
    arr = "ARRAY[" + ",".join(f"'{t}'" for t in TYPES) + "]"
    sql = f"""
SELECT o.oprname || '|' || COALESCE(lt.typname,'') || '|' || COALESCE(rt.typname,'')
FROM pg_operator o
LEFT JOIN pg_type lt ON lt.oid = o.oprleft
LEFT JOIN pg_type rt ON rt.oid = o.oprright
WHERE o.oprnamespace = 'pg_catalog'::regnamespace
  AND (lt.typname IS NULL OR lt.typname = ANY({arr}))
  AND (rt.typname IS NULL OR rt.typname = ANY({arr}))
ORDER BY o.oprname, lt.typname, rt.typname;
"""
    out, err = psql_batch(u, sql, password=password)
    if not out.strip():
        raise SystemExit(f"could not list operators: {err[:400]}")
    return [ln for ln in out.splitlines() if ln.count("|") == 2]


def build_cases(rows):
    seen, cases = set(), []
    for row in rows:
        op, lt, rt = row.split("|")
        if not rt:
            continue  # postfix operators were removed in PG14
        # Wrapped in a CASE so a RESOLVED operator always prints a
        # non-empty token. Bare `SELECT NULL::int4 + NULL::int4` prints an
        # empty line — NULL renders as nothing under `-A -t` — which is
        # indistinguishable from a statement that errored and printed no
        # row at all. A first pass of this sweep used that as its signal
        # and duly reported every one of 557 forms as refused on BOTH
        # servers, which is how the planted canaries earn their keep.
        operand = f"NULL::{lt} " if lt else ""
        e = f"SELECT CASE WHEN ({operand}{op} NULL::{rt}) IS NULL THEN 'null' ELSE 'nonnull' END"
        if e in seen:
            continue
        seen.add(e)
        cases.append((op, lt or "-", rt, e))
    return cases


def run_cases(u, cases, password=None):
    """Return {index: (ok, text)} — one session, markers between statements."""
    lines = ["\\set ON_ERROR_STOP off"]
    for i, (_, _, _, e) in enumerate(cases):
        lines.append(f"\\echo {MARK} {i}")
        lines.append(e + ";")
    out, err = psql_batch(u, "\n".join(lines), password=password)
    # psql writes \echo to stdout and errors to stderr; interleave by
    # re-reading with 2>&1 semantics is not available, so ask again with
    # errors folded into stdout.
    return parse(out, err, len(cases))


def parse(out, err, n):
    """Fold the marker-delimited stdout and the error stream into per-case
    outcomes. Errors carry no marker, so they are matched by ORDER: psql
    emits them in statement order, and every failing statement produces
    exactly one ERROR line."""
    results = {}
    cur, buf = None, []
    for line in out.splitlines():
        if line.startswith(MARK):
            if cur is not None:
                results[cur] = "\n".join(buf).strip()
            cur = int(line.split()[1])
            buf = []
        elif cur is not None:
            buf.append(line)
    if cur is not None:
        results[cur] = "\n".join(buf).strip()

    errors = [ln for ln in err.splitlines() if ln.startswith("ERROR:")]
    # A case that produced no stdout rows is the one that errored; walk in
    # order and pair them up.
    failed = [i for i in range(n) if not results.get(i)]
    out_map = {}
    for i in range(n):
        if i in failed:
            k = failed.index(i)
            out_map[i] = (False, errors[k] if k < len(errors) else "ERROR: (unpaired)")
        else:
            out_map[i] = (True, results[i])
    return out_map


def main():
    pg_host, pg_port, spg_host, spg_port = sys.argv[1:5]
    pg_u = url(pg_host, pg_port, "bench", "bench")
    spg_u = url(spg_host, spg_port, "postgres", "unmei")

    cases = build_cases(list_operators(pg_u, password="bench"))
    # A sweep that reports nothing has to first prove it can report
    # something. These two are planted: PG resolves neither, and SPG is
    # known to answer the second one (r507 gave it unary `+`), so the
    # harness must classify them as it would any real case.
    cases.append(("@#$", "int4", "int4",
                  "SELECT CASE WHEN (NULL::int4 @#$ NULL::int4) IS NULL THEN 'null' ELSE 'x' END"))
    cases.append(("<<canary>>", "-", "int4", "SELECT !!!bogus!!!"))
    print(f"# {len(cases)} operator forms from pg_operator (last 2 are planted)")

    pg_res = run_cases(pg_u, cases, password="bench")
    spg_res = run_cases(spg_u, cases)

    missing, differing = [], []
    for i, (op, lt, rt, _) in enumerate(cases):
        p_ok, p_msg = pg_res[i]
        s_ok, s_msg = spg_res[i]
        if p_ok and not s_ok:
            missing.append((op, lt, rt, s_msg))
        elif p_ok and s_ok and p_msg != s_msg:
            differing.append((op, lt, rt, p_msg, s_msg))

    pg_err = sum(1 for i in range(len(cases)) if not pg_res[i][0])
    spg_err = sum(1 for i in range(len(cases)) if not spg_res[i][0])
    print(f"# refused: PG {pg_err}, SPG {spg_err} (of {len(cases)})")
    if pg_err < 2:
        print("!! the planted cases did not register — the harness is not "
              "seeing errors, so a clean report means nothing")
    print(f"\n## PG resolves it, SPG does not — {len(missing)}")
    for op, lt, rt, msg in missing:
        print(f"  {lt:>12} {op:<6} {rt:<12}  {msg[:90]}")
    print(f"\n## both resolve it, answers differ — {len(differing)}")
    for op, lt, rt, a, b in differing:
        print(f"  {lt:>12} {op:<6} {rt:<12}  PG={a[:28]!r} SPG={b[:28]!r}")


if __name__ == "__main__":
    main()
