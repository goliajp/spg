#!/usr/bin/env python3
"""v7.39 (round 520) — the constant-answer test, generated rather than typed.

`constant-answer-probe.py` asks 31 hand-written pairs and found nine stubs.
Its value is entirely in how many pairs it asks, and thirty-one is thin
against the ~760 functions SPG resolves — so this generates the pairs from
`pg_proc` instead of from what I happened to think of.

The test is the same one: a stub is invisible when you call it once and
obvious when you call it TWICE with inputs that should disagree. Here the
two inputs come from the DECLARED argument types — a text parameter gets
'alpha' and 'zeta', an integer 1 and 77 — and a function is suspect when SPG
answers the same thing both times while PG does not.

Only a PG DISAGREEMENT makes a case count, which is what keeps a function
that genuinely ignores its arguments (`pg_backend_pid`, say) from being
reported. Anything PG answers the same way twice is dropped before SPG is
even asked.

  constant-answer-sweep.py <pg_host> <pg_port> <spg_host> <spg_port> [limit]
"""

import subprocess
import sys

MARK = "@@CASE"

# Two literals per type, chosen to land in different answers wherever the
# function reads its argument at all.
LITERALS = {
    "text": ("'alpha'", "'zeta'"),
    "character varying": ("'alpha'", "'zeta'"),
    "name": ("'alpha'", "'zeta'"),
    "\"char\"": ("'a'", "'z'"),
    "integer": ("1", "77"),
    "smallint": ("1::smallint", "77::smallint"),
    "bigint": ("1::bigint", "77::bigint"),
    "oid": ("1::oid", "77::oid"),
    "real": ("1.5::real", "77.5::real"),
    "double precision": ("1.5::float8", "77.5::float8"),
    "numeric": ("1.5", "77.5"),
    "boolean": ("true", "false"),
    "date": ("DATE '2020-01-01'", "DATE '2024-06-15'"),
    "timestamp without time zone": ("TIMESTAMP '2020-01-01'", "TIMESTAMP '2024-06-15'"),
    "timestamp with time zone": ("TIMESTAMPTZ '2020-01-01Z'", "TIMESTAMPTZ '2024-06-15Z'"),
    "interval": ("INTERVAL '1 day'", "INTERVAL '77 days'"),
    "time without time zone": ("TIME '01:00'", "TIME '17:30'"),
    "bytea": ("'\\\\x01'::bytea", "'\\\\x7f7f'::bytea"),
    "uuid": ("'00000000-0000-0000-0000-000000000001'::uuid",
             "'ffffffff-ffff-ffff-ffff-ffffffffffff'::uuid"),
    "json": ("'{\"a\":1}'::json", "'{\"z\":77}'::json"),
    "jsonb": ("'{\"a\":1}'::jsonb", "'{\"z\":77}'::jsonb"),
    "inet": ("'10.0.0.1'::inet", "'192.168.9.9'::inet"),
    "cidr": ("'10.0.0.0/8'::cidr", "'192.168.0.0/16'::cidr"),
    "tsvector": ("'cat:1'::tsvector", "'zebra:9'::tsvector"),
    "tsquery": ("'cat'::tsquery", "'zebra'::tsquery"),
    "xml": ("'<a/>'::xml", "'<zz/>'::xml"),
    "money": ("'1.00'::money", "'77.00'::money"),
    "bit": ("B'1'", "B'0'"),
    "point": ("point '(0,0)'", "point '(9,9)'"),
    "box": ("box '((0,0),(1,1))'", "box '((7,7),(9,9))'"),
    "lseg": ("lseg '((0,0),(1,1))'", "lseg '((7,7),(9,9))'"),
    "circle": ("circle '((0,0),1)'", "circle '((9,9),7)'"),
    "path": ("path '((0,0),(1,1))'", "path '((7,7),(9,9))'"),
    "polygon": ("polygon '((0,0),(1,1),(1,0))'", "polygon '((7,7),(9,9),(9,7))'"),
    "macaddr": ("'08:00:2b:01:02:03'::macaddr", "'ff:ff:ff:ff:ff:ff'::macaddr"),
}

LIST_SQL = """
SELECT p.proname || '|' || pg_catalog.pg_get_function_arguments(p.oid)
FROM pg_proc p
WHERE p.pronamespace = 'pg_catalog'::regnamespace
  AND p.prokind = 'f'
  AND p.provariadic = 0
  AND NOT p.proretset
  AND p.pronargs BETWEEN 1 AND 3
ORDER BY p.proname, p.oid;
"""

# Same deny-list reasoning as the function sweep: a call that could DO
# something, or whose absence is a deployment fact.
DENY = (
    "pg_terminate_ pg_cancel_ pg_reload_ pg_rotate_ pg_switch_ pg_create_ pg_drop_ "
    "pg_promote pg_stat_reset pg_replication_ pg_log_ pg_backup_ pg_wal_replay "
    "pg_import_ set_config lo_ pg_sleep pg_advisory pg_ls_ pg_read_ pg_stat_file "
    "pg_tablespace_ pg_signal_ binary_upgrade_ pg_event_trigger pg_extension "
    "random gen_random setseed"
).split()


def psql(u, script, password=None, timeout=1800):
    env = ["-e", f"PGPASSWORD={password}"] if password else []
    r = subprocess.run(
        ["docker", "run", "--rm", "-i", *env, "postgres:18-alpine", "psql", u, "-A", "-t"],
        input=script, capture_output=True, text=True, timeout=timeout,
    )
    return r.stdout or "", r.stderr or ""


def url(host, port, db, user):
    return f"postgres://{user}@{host}:{port}/{db}?sslmode=disable"


def build_cases(rows, limit):
    seen, cases = set(), []
    for row in rows:
        name, argsig = row.split("|", 1)
        if any(name.startswith(d) for d in DENY):
            continue
        types = [p.strip().split(" ", 1)[-1].strip() for p in argsig.split(",") if p.strip()]
        # Every argument must have a pair, and at least one must differ.
        if not types or any(t not in LITERALS for t in types):
            continue
        a = ", ".join(LITERALS[t][0] for t in types)
        b = ", ".join(LITERALS[t][1] for t in types)
        key = f"{name}({a})"
        if key in seen:
            continue
        seen.add(key)
        # Wrapped so a SUCCESS always prints a non-empty token. A statement
        # that errors prints nothing to stdout, and a bare `SELECT f(...)`
        # that answers NULL prints an empty line — indistinguishable. A first
        # run of this sweep read 246 "constants" that were really functions
        # SPG does not have. Round 508's operator sweep learned the same
        # thing; this is the third time the trap has come up.
        cases.append((
            f"{name}({argsig})",
            f"SELECT 'v=' || coalesce(({name}({a}))::text, '<null>')",
            f"SELECT 'v=' || coalesce(({name}({b}))::text, '<null>')",
        ))
        if limit and len(cases) >= limit:
            break
    return cases


def run_batch(u, stmts, password=None):
    lines = ["\\set ON_ERROR_STOP off"]
    for i, sql in enumerate(stmts):
        lines.append(f"\\echo {MARK} {i}")
        lines.append(sql + ";")
    out, _ = psql(u, "\n".join(lines), password=password)
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
    # Anything that did not print the `v=` prefix did not run.
    return [
        results[i] if results.get(i, "").startswith("v=") else "<err>"
        for i in range(len(stmts))
    ]


def main():
    pg_host, pg_port, spg_host, spg_port = sys.argv[1:5]
    limit = int(sys.argv[5]) if len(sys.argv) > 5 else 0
    pg_u = url(pg_host, pg_port, "bench", "bench")
    spg_u = url(spg_host, spg_port, "postgres", "unmei")

    out, err = psql(pg_u, LIST_SQL, password="bench")
    if not out.strip():
        raise SystemExit(f"could not list: {err[:300]}")
    cases = build_cases([l for l in out.splitlines() if "|" in l], limit)
    print(f"# {len(cases)} generated pairs")

    a_sql = [a for _, a, _ in cases]
    b_sql = [b for _, _, b in cases]
    pa = run_batch(pg_u, a_sql, "bench")
    pb = run_batch(pg_u, b_sql, "bench")
    sa = run_batch(spg_u, a_sql)
    sb = run_batch(spg_u, b_sql)

    suspects = []
    for i, (label, _, _) in enumerate(cases):
        # PG must distinguish the pair, and answer both.
        if pa[i] == pb[i] or "<err>" in (pa[i], pb[i]):
            continue
        if sa[i] == "<err>" or sb[i] == "<err>":
            continue  # not resolvable here — that is the other sweep's job
        if sa[i] == sb[i]:
            suspects.append((label, pa[i], pb[i], sa[i]))
    print(f"# {len(suspects)} answered the same to both inputs where PG did not\n")
    for label, x, y, same in suspects:
        print(f"  {label}\n      PG {x[:28]!r} vs {y[:28]!r}   SPG {same[:28]!r} both")


if __name__ == "__main__":
    main()
