#!/usr/bin/env python3
"""v7.39 (round 507) — can a deeply nested statement abort the SERVER?

`MAX_NEST_DEPTH` turns a deep statement into a catchable parse error. If any
recursive shape escapes that budget, the stack goes instead — and a stack
overflow is an abort, which in the server does not fail one query, it takes
the process down and every other connection with it. That is the severity
question, and it can only be answered against a running server.

  nest-depth-server-probe.py <host> <port>
"""

import subprocess
import sys


def main():
    host, port = sys.argv[1], sys.argv[2]
    url = f"postgres://unmei@{host}:{port}/postgres?sslmode=disable"

    def q(sql, timeout=180):
        # On stdin, not `-c`: the long shapes blow the exec argument limit,
        # which looks like a failure but never reaches the server.
        return subprocess.run(
            ["docker", "run", "--rm", "-i", "postgres:18-alpine",
             "psql", url, "-A", "-t"],
            input=sql, capture_output=True, text=True, timeout=timeout,
        )

    q("DROP TABLE IF EXISTS lbl")
    q("CREATE TABLE lbl(a INT)")
    q("INSERT INTO lbl VALUES (1)")

    shapes = [
        ("derived-200", "SELECT * FROM (" * 200 + "SELECT a FROM lbl" + ") t" * 200),
        ("parens-500", "SELECT " + "(" * 500 + "1" + ")" * 500),
        ("calls-500", "SELECT " + "upper(" * 500 + "'x'" + ")" * 500),
        ("in_subq-200", "SELECT a FROM lbl WHERE a IN (" * 200 + "SELECT a FROM lbl" + ")" * 200),
        ("not-500", "SELECT " + "NOT " * 500 + "TRUE"),
        ("case-200", "SELECT " + "CASE WHEN a=1 THEN " * 200 + "1" + " ELSE 0 END" * 200 + " FROM lbl"),
        ("scalar_subq-200", "SELECT (" * 200 + "SELECT a FROM lbl LIMIT 1" + ")" * 200),
        ("cast-2000", "SELECT 1" + "::text" * 2000),
        ("arith-5000", "SELECT 1" + " + 1" * 5000),
        ("union-5000", "SELECT a FROM lbl" + " UNION ALL SELECT a FROM lbl" * 5000),
        ("cte-2000", build_cte(2000)),
        # Spaced, because `--` is a LINE COMMENT: an unspaced run of minus
        # signs comments the statement out and proves nothing about unary
        # minus, which is what this is actually asking about.
        ("neg-2000", "SELECT " + "- " * 2000 + "1"),
        ("plus-2000", "SELECT " + "+ " * 2000 + "1"),
    ]
    for name, sql in shapes:
        try:
            r = q(sql)
            out = (r.stdout + r.stderr).strip().replace("\n", " ")[:66]
        except subprocess.TimeoutExpired:
            out = "TIMEOUT"
            r = None
        rc = "?" if r is None else r.returncode
        print(f"{name:>16}: rc={rc} {out}")
        # The point of the whole probe: is the server still there?
        alive = q("SELECT 42")
        if alive.returncode != 0 or alive.stdout.strip() != "42":
            print(f"  *** SERVER DIED after {name} ***")
            return
    print("server alive and answering after every shape")


def build_cte(n):
    parts = ["WITH c0 AS (SELECT a FROM lbl)"]
    for i in range(1, n):
        parts.append(f", c{i} AS (SELECT a FROM c{i - 1})")
    parts.append(f" SELECT a FROM c{n - 1}")
    return "".join(parts)


if __name__ == "__main__":
    main()
