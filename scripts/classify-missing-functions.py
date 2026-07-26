#!/usr/bin/env python3
"""v7.39 (round 509) — sort the function-sweep's misses by what they ARE.

`function-surface-diff.py` reports every `pg_proc` entry SPG cannot resolve,
and the raw count is close to nine hundred — a number that says nothing on
its own, because most of `pg_catalog` is not application surface. PG's own
catalogue knows which is which:

  * operator-impl — the C function BEHIND an operator (`int4pl` is `+`).
    Callable, but applications write the operator.
  * type-io       — a type's input/output/send/receive (`int4in`).
  * index-support — an access method's support routine (`btint4cmp`).
  * user-facing   — everything else. This is the number that matters.

  classify-missing-functions.py <names-file>
"""

import subprocess
import sys

SQL = """
WITH miss(name) AS (VALUES {vals})
SELECT kind, count(*), string_agg(name, ' ' ORDER BY name)
FROM (
  SELECT name, CASE
    WHEN EXISTS (SELECT 1 FROM pg_operator o JOIN pg_proc p ON p.oid = o.oprcode
                 WHERE p.proname = miss.name) THEN 'operator-impl'
    WHEN EXISTS (SELECT 1 FROM pg_type t
                 WHERE miss.name IN (t.typinput::text, t.typoutput::text,
                                     t.typsend::text, t.typreceive::text,
                                     t.typmodin::text, t.typmodout::text))
         THEN 'type-io'
    WHEN EXISTS (SELECT 1 FROM pg_amproc a JOIN pg_proc p ON p.oid = a.amproc
                 WHERE p.proname = miss.name) THEN 'index-support'
    WHEN EXISTS (SELECT 1 FROM pg_cast c JOIN pg_proc p ON p.oid = c.castfunc
                 WHERE p.proname = miss.name) THEN 'cast-impl'
    ELSE 'user-facing'
  END AS kind
  FROM miss
) s
GROUP BY kind ORDER BY 2 DESC;
"""


def main():
    names = sorted(set(open(sys.argv[1]).read().split()))
    vals = ",".join("('%s')" % n for n in names if n.replace("_", "").isalnum())
    r = subprocess.run(
        ["docker", "exec", "-e", "PGPASSWORD=bench", "-i", "spg-bench-postgres",
         "psql", "-U", "bench", "-d", "bench", "-A", "-t"],
        input=SQL.format(vals=vals), capture_output=True, text=True,
    )
    if not r.stdout.strip():
        print(r.stderr[:500])
        return
    for line in r.stdout.strip().splitlines():
        kind, count, members = line.split("|", 2)
        print(f"\n## {kind} — {count}")
        if kind == "user-facing":
            words = members.split()
            for i in range(0, len(words), 6):
                print("   " + " ".join(words[i:i + 6]))


if __name__ == "__main__":
    main()
