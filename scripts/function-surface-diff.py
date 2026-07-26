#!/usr/bin/env python3
"""v7.39 (round 509) — which of PG18's functions does SPG not have?

The operator sweep (round 508) took its list from `pg_operator` and found 33
gaps that nothing else had caught. `pg_proc` is the same kind of ground
truth for functions, so this asks it the same way: call each function with
NULL arguments of its declared types, and see whether the server RESOLVES
it. Value-domain errors never enter into it.

The two planted canaries are not decoration. The operator sweep's first run
reported zero gaps, and it was wrong — `psql -A -t` prints NULL as an empty
line, and the harness had been reading "no output" as "errored", so every
successful case scored as a failure and the totals cancelled out. Anything
that reports a clean sweep has to first prove it can report a dirty one.

  function-surface-diff.py <pg_host> <pg_port> <spg_host> <spg_port>
"""

import subprocess
import sys

MARK = "@@CASE"

# Functions whose NULL call could still do something, or whose absence is a
# deployment fact rather than a capability gap.
DENY = (
    "pg_terminate_ pg_cancel_ pg_reload_ pg_rotate_ pg_switch_ pg_create_ "
    "pg_drop_ pg_promote pg_stat_reset pg_replication_ pg_log_ pg_backup_ "
    "pg_wal_replay pg_import_ set_config lo_ pg_sleep pg_advisory pg_relation_ "
    "pg_ls_ pg_read_ pg_stat_file pg_tablespace_ pg_signal_ binary_upgrade_"
).split()


def url(host, port, db, user):
    return f"postgres://{user}@{host}:{port}/{db}?sslmode=disable"


def psql_batch(u, script, timeout=1800, password=None):
    env = ["-e", f"PGPASSWORD={password}"] if password else []
    r = subprocess.run(
        ["docker", "run", "--rm", "-i", *env, "postgres:18-alpine", "psql", u, "-A", "-t"],
        input=script, capture_output=True, text=True, timeout=timeout,
    )
    return r.stdout or "", r.stderr or ""


LIST_SQL = """
SELECT p.proname || '|' || pg_catalog.pg_get_function_arguments(p.oid)
FROM pg_proc p
WHERE p.pronamespace = 'pg_catalog'::regnamespace
  AND p.prokind = 'f'
  AND p.provariadic = 0
  AND NOT p.proretset
  AND p.pronargs <= 4
  AND p.proargtypes::text <> ''
  AND NOT EXISTS (
    SELECT 1 FROM unnest(p.proargtypes) t(oid)
    JOIN pg_type ty ON ty.oid = t.oid
    WHERE ty.typname LIKE 'any%' OR ty.typtype IN ('p','c')
  )
ORDER BY p.proname, p.oid;
"""


def list_functions(u, password=None):
    out, err = psql_batch(u, LIST_SQL, password=password)
    if not out.strip():
        raise SystemExit(f"could not list functions: {err[:400]}")
    return [ln for ln in out.splitlines() if "|" in ln]


def build_cases(rows):
    seen, cases = set(), []
    for row in rows:
        name, argsig = row.split("|", 1)
        if any(name.startswith(d) for d in DENY):
            continue
        # `pg_get_function_arguments` gives "a integer, b text"; the type is
        # the last word of each part, which is all a NULL call needs.
        types = []
        ok = True
        for part in argsig.split(","):
            part = part.strip()
            if not part or " " not in part and part.count(" ") == 0 and not part:
                ok = False
                break
            tok = part.split()[-1]
            if "[]" in tok or not tok:
                tok = tok or ""
            types.append(tok)
        if not ok or not types:
            continue
        call = f"{name}(" + ", ".join(f"NULL::{t}" for t in types) + ")"
        stmt = f"SELECT CASE WHEN ({call}) IS NULL THEN 'null' ELSE 'nonnull' END"
        if stmt in seen:
            continue
        seen.add(stmt)
        cases.append((name, argsig, stmt))
    return cases


def run_cases(u, cases, password=None):
    lines = ["\\set ON_ERROR_STOP off"]
    for i, (_, _, stmt) in enumerate(cases):
        lines.append(f"\\echo {MARK} {i}")
        lines.append(stmt + ";")
    out, err = psql_batch(u, "\n".join(lines), password=password)
    return parse(out, err, len(cases))


def parse(out, err, n):
    results, cur, buf = {}, None, []
    for line in out.splitlines():
        if line.startswith(MARK):
            if cur is not None:
                results[cur] = "\n".join(buf).strip()
            cur, buf = int(line.split()[1]), []
        elif cur is not None:
            buf.append(line)
    if cur is not None:
        results[cur] = "\n".join(buf).strip()

    errors = [ln for ln in err.splitlines() if ln.startswith("ERROR:")]
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

    cases = build_cases(list_functions(pg_u, password="bench"))
    cases.append(("__canary_bogus", "()",
                  "SELECT CASE WHEN (spg_no_such_fn(NULL::int4)) IS NULL "
                  "THEN 'null' ELSE 'x' END"))
    cases.append(("__canary_syntax", "()", "SELECT !!!bogus!!!"))
    print(f"# {len(cases)} function forms from pg_proc (last 2 are planted)")

    pg_res = run_cases(pg_u, cases, password="bench")
    spg_res = run_cases(spg_u, cases)

    pg_err = sum(1 for i in range(len(cases)) if not pg_res[i][0])
    spg_err = sum(1 for i in range(len(cases)) if not spg_res[i][0])
    print(f"# refused: PG {pg_err}, SPG {spg_err} (of {len(cases)})")
    if pg_err < 2:
        print("!! the planted cases did not register — the harness is not "
              "seeing errors, so a clean report means nothing")

    missing = []
    for i, (name, argsig, _) in enumerate(cases):
        if pg_res[i][0] and not spg_res[i][0]:
            missing.append((name, argsig, spg_res[i][1]))

    print(f"\n## PG resolves it, SPG does not — {len(missing)}")
    for name, argsig, msg in missing:
        print(f"  {name}({argsig})\n      {msg[:100]}")


if __name__ == "__main__":
    main()
