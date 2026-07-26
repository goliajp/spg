#!/usr/bin/env python3
"""v7.39 (round 516) — ask the user-facing misses with REAL arguments.

`function-surface-diff.py` calls everything with NULLs, which is right for
"does the server resolve this" and wrong for deciding what to implement: a
function SPG has can still refuse an all-NULL call, and twice now the sweep
has reported one missing on that basis alone (round 510's `ts_rank`, and
`setweight` / `json_to_record` here). The reverse also happens — `setweight`
resolves, but the sweep was probing a 3-argument overload SPG lacks.

So this panel calls each candidate the way an application would, and diffs
the two servers on the ANSWER. A row where both agree needs nothing.

  user-facing-fn-diff.py <pg_host> <pg_port> <spg_host> <spg_port>
"""

import subprocess
import sys

# Each entry is a statement an application could plausibly write.
PANEL = [
    ("setweight/2", "SELECT setweight('cat:1 dog:2'::tsvector, 'B')::text"),
    ("setweight/3", "SELECT setweight('cat:1 dog:2'::tsvector, 'B', '{cat}')::text"),
    ("json_to_record", "SELECT a FROM json_to_record('{\"a\":1}') AS r(a int)"),
    ("jsonb_to_record", "SELECT a FROM jsonb_to_record('{\"a\":1}'::jsonb) AS r(a int)"),
    ("isparallel", "SELECT isparallel(lseg '((0,0),(1,1))', lseg '((0,1),(1,2))')"),
    ("isperp", "SELECT isperp(lseg '((0,0),(1,1))', lseg '((0,1),(1,0))')"),
    ("to_regcollation", "SELECT to_regcollation('\"C\"')::text"),
    ("to_regcollation/miss", "SELECT to_regcollation('nosuch') IS NULL"),
    ("unicode_assigned", "SELECT unicode_assigned('abc')"),
    ("oidvectortypes", "SELECT oidvectortypes('23 25'::oidvector)"),
    ("textlen", "SELECT textlen('abc')"),
    ("nameconcatoid", "SELECT nameconcatoid('abc', 42)"),
    ("numeric_ln", "SELECT numeric_ln(2.0)::text"),
    ("numeric_sqrt", "SELECT numeric_sqrt(4.0)::text"),
    ("numeric_exp", "SELECT round(numeric_exp(1.0), 4)::text"),
    ("numeric_log", "SELECT numeric_log(10.0, 100.0)::text"),
    ("numeric_div_trunc", "SELECT numeric_div_trunc(7.0, 2.0)::text"),
    ("int8inc", "SELECT int8inc(1::bigint)"),
    ("int8dec", "SELECT int8dec(1::bigint)"),
    ("has_largeobject_privilege", "SELECT has_largeobject_privilege(1::oid, 'SELECT')"),
    ("txid_visible_in_snapshot",
     "SELECT txid_visible_in_snapshot(1::bigint, '10:20:'::txid_snapshot)"),
    ("pg_collation_is_visible", "SELECT pg_collation_is_visible(100::oid) IS NOT NULL"),
    ("pg_conversion_is_visible", "SELECT pg_conversion_is_visible(100::oid) IS NOT NULL"),
    ("pg_opfamily_is_visible", "SELECT pg_opfamily_is_visible(403::oid) IS NOT NULL"),
    ("pg_is_other_temp_schema", "SELECT pg_is_other_temp_schema(11::oid)"),
    ("pg_settings_get_flags", "SELECT pg_settings_get_flags('work_mem')::text"),
    ("pg_stat_have_stats", "SELECT pg_stat_have_stats('database', 0::oid, 0::oid) IS NOT NULL"),
    ("xmlconcat2", "SELECT xmlconcat2('<a/>'::xml, '<b/>'::xml)::text"),
    ("oidlarger", "SELECT oidlarger(1::oid, 2::oid)"),
    ("oidsmaller", "SELECT oidsmaller(1::oid, 2::oid)"),
    ("network_larger", "SELECT network_larger('10.0.0.1'::inet, '10.0.0.2'::inet)::text"),
    ("bpchar_larger", "SELECT bpchar_larger('a'::bpchar, 'b'::bpchar)::text"),
    ("tidlarger", "SELECT tidlarger('(0,1)'::tid, '(0,2)'::tid)::text"),
    ("currtid2", "SELECT currtid2('t', '(0,1)'::tid)::text"),
    ("to_regtypemod", "SELECT to_regtypemod('varchar(32)')"),
    ("pg_get_acl", "SELECT pg_get_acl('pg_class'::regclass, 0::oid, 0) IS NOT NULL"),
]


def run(host, port, db, user, password, sql):
    url = f"postgres://{user}@{host}:{port}/{db}?sslmode=disable"
    env = ["-e", f"PGPASSWORD={password}"] if password else []
    r = subprocess.run(
        ["docker", "run", "--rm", "-i", *env, "postgres:18-alpine", "psql", url, "-A", "-t", "-c", sql],
        capture_output=True, text=True, timeout=120,
    )
    if r.returncode != 0:
        first = (r.stderr or "").splitlines()
        return "ERR " + (first[0].replace("ERROR:  ", "") if first else "?")
    return (r.stdout or "").strip().replace("\n", " ") or "<null>"


def main():
    pg_host, pg_port, spg_host, spg_port = sys.argv[1:5]
    same = 0
    print(f"{'case':<28} {'PG18':<34} SPG")
    for name, sql in PANEL:
        a = run(pg_host, pg_port, "bench", "bench", "bench", sql)
        b = run(spg_host, spg_port, "postgres", "unmei", None, sql)
        if a == b:
            same += 1
            continue
        print(f"{name:<28} {a[:33]:<34} {b[:60]}")
    print(f"\n# {same}/{len(PANEL)} already agree")


if __name__ == "__main__":
    main()
