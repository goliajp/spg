#!/usr/bin/env python3
"""v7.39 (round 521) — do the suite's own SELECTs agree with PG18?

Five rounds have now found a test asserting SPG's output rather than PG's
answer (r392, r504, r517, r518, r520), every one of them by accident — the
test only surfaced because a fix made it fail. A test that pins our own
behaviour is worse than no test: it makes the wrong answer load-bearing.

So this asks the question directly. Every `SELECT …` string literal in the
e2e suite is run against BOTH servers and the answers compared. A
disagreement is one of two things, and both are worth seeing: a divergence
somebody chose and wrote down, or an expectation that was never PG's.

It cannot judge which — that is the reading. It can stop the finding from
depending on a fix happening to break something.

  test-expectation-audit.py <pg_host> <pg_port> <spg_host> <spg_port> [limit]
"""

import pathlib
import re
import subprocess
import sys

MARK = "@@CASE"
TEST_DIRS = [
    "crates/spg-engine/tests/e2e",
    "crates/spg-server/tests/e2e",
]

# A SELECT inside a Rust string literal. Rust escapes `\"` inside, and the
# suite writes plenty of those.
SELECT_RE = re.compile(r'"((?:SELECT|select)\s(?:[^"\\]|\\.)*)"')

# Statements that cannot be judged by re-running them: they read the clock,
# the session, or state a bare server does not have.
SKIP = re.compile(
    r"\b(now|clock_timestamp|statement_timestamp|current_timestamp|current_date|"
    r"current_time|random|uuid|nextval|currval|lastval|pg_backend_pid|"
    r"txid_current|pg_current_snapshot|txid_current_snapshot|version|"
    r"current_user|session_user|system_user|current_database|current_schema|"
    r"pg_postmaster_start_time|pg_conf_load_time|age)\b",
    re.I,
)


def collect(limit):
    seen, out = set(), []
    for d in TEST_DIRS:
        for path in sorted(pathlib.Path(d).glob("*.rs")):
            for m in SELECT_RE.finditer(path.read_text()):
                sql = m.group(1).replace('\\"', '"').replace("\\\\", "\\")
                if "{" in sql or "}" in sql:
                    continue  # a format! template, not a statement
                if SKIP.search(sql) or sql in seen:
                    continue
                seen.add(sql)
                out.append((path.name, sql))
                if limit and len(out) >= limit:
                    return out
    return out


def psql(u, script, password=None, timeout=1800):
    env = ["-e", f"PGPASSWORD={password}"] if password else []
    r = subprocess.run(
        ["docker", "run", "--rm", "-i", *env, "postgres:18-alpine", "psql", u, "-A", "-t"],
        input=script, capture_output=True, text=True, timeout=timeout,
    )
    return r.stdout or ""


def run_batch(u, stmts, password=None):
    lines = ["\\set ON_ERROR_STOP off"]
    for i, sql in enumerate(stmts):
        lines.append(f"\\echo {MARK} {i}")
        lines.append(sql.rstrip().rstrip(";") + ";")
        # A marker after each too, so a statement that printed nothing is
        # distinguishable from one whose output ran into the next marker.
    out = psql(u, "\n".join(lines), password=password)
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
    return [results.get(i, "<none>") for i in range(len(stmts))]


def main():
    pg_host, pg_port, spg_host, spg_port = sys.argv[1:5]
    limit = int(sys.argv[5]) if len(sys.argv) > 5 else 0
    pg_u = f"postgres://bench@{pg_host}:{pg_port}/bench?sslmode=disable"
    spg_u = f"postgres://unmei@{spg_host}:{spg_port}/postgres?sslmode=disable"

    cases = collect(limit)
    print(f"# {len(cases)} SELECT literals from the e2e suite")
    stmts = [s for _, s in cases]
    pg = run_batch(pg_u, stmts, "bench")
    spg = run_batch(spg_u, stmts)

    both_ran = differ = 0
    for (fname, sql), a, b in zip(cases, pg, spg):
        # Only statements BOTH servers answered can be compared. One that
        # errors on PG is usually SPG-specific syntax; one that errors on
        # SPG is the function sweep's business, not this audit's.
        if a == "<none>" or b == "<none>" or not a or not b:
            continue
        both_ran += 1
        if a != b:
            differ += 1
            print(f"\n  {fname}\n    {sql[:96]}")
            print(f"      PG18 {a[:60]!r}")
            print(f"      SPG  {b[:60]!r}")
    print(f"\n# {both_ran} comparable, {differ} disagree")


if __name__ == "__main__":
    main()
