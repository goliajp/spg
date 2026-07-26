#!/usr/bin/env python3
"""v7.39 (round 518) — find functions that answer a CONSTANT.

Round 517 found `pg_visible_in_snapshot` returning TRUE for everything under
a comment reading "no MVCC-yet model" — a stub that outlived its reason by
several releases, and that a caller could not tell from a real answer. It
was found by accident, because adding a second spelling made the compiler
report an unreachable arm.

This looks for the rest of them mechanically. A stub is invisible when you
call it once; it shows the moment you call it TWICE with inputs that should
disagree. So each case here is a pair, and a function is suspect when SPG
answers the same thing both times while PG does not.

That test has no false negatives worth worrying about and one obvious false
positive — a function that genuinely returns the same value for both inputs
— which is why PG runs the same pair and only a DISAGREEMENT is reported.

  constant-answer-probe.py <pg_host> <pg_port> <spg_host> <spg_port>
"""

import subprocess
import sys

# (label, sql_a, sql_b) — two calls whose answers PG distinguishes.
PAIRS = [
    ("pg_visible_in_snapshot",
     "SELECT pg_visible_in_snapshot('5'::xid8,'10:20:'::pg_snapshot)",
     "SELECT pg_visible_in_snapshot('25'::xid8,'10:20:'::pg_snapshot)"),
    ("txid_status", "SELECT txid_status(1::bigint)", "SELECT txid_status(999999999::bigint)"),
    ("pg_column_size", "SELECT pg_column_size(1::int)", "SELECT pg_column_size('abcdefghij'::text)"),
    ("pg_database_size", "SELECT pg_database_size(current_database()) > 0",
     "SELECT pg_database_size(current_database()) > 1e15"),
    ("pg_total_relation_size", "SELECT pg_total_relation_size('pg_class'::regclass) >= 0",
     "SELECT pg_total_relation_size('pg_class'::regclass) > 1e15"),
    ("has_table_privilege", "SELECT has_table_privilege('pg_class','SELECT')",
     "SELECT has_table_privilege('pg_class','INSERT')"),
    ("has_schema_privilege", "SELECT has_schema_privilege('pg_catalog','USAGE')",
     "SELECT has_schema_privilege('pg_catalog','CREATE')"),
    ("pg_table_is_visible", "SELECT pg_table_is_visible('pg_class'::regclass)",
     "SELECT pg_table_is_visible(999999::oid)"),
    ("pg_type_is_visible", "SELECT pg_type_is_visible('int4'::regtype)",
     "SELECT pg_type_is_visible(999999::oid)"),
    ("pg_function_is_visible", "SELECT pg_function_is_visible('abs(int)'::regprocedure)",
     "SELECT pg_function_is_visible(999999::oid)"),
    ("pg_get_expr", "SELECT pg_get_expr(NULL, 0::oid) IS NULL",
     "SELECT pg_get_expr(NULL, 0::oid) IS NOT NULL"),
    ("pg_encoding_to_char", "SELECT pg_encoding_to_char(6)", "SELECT pg_encoding_to_char(0)"),
    ("pg_char_to_encoding", "SELECT pg_char_to_encoding('UTF8')",
     "SELECT pg_char_to_encoding('SQL_ASCII')"),
    ("pg_backend_pid", "SELECT pg_backend_pid() > 0", "SELECT pg_backend_pid() < 0"),
    ("pg_is_in_recovery", "SELECT pg_is_in_recovery()", "SELECT NOT pg_is_in_recovery()"),
    ("txid_current_snapshot",
     "SELECT txid_current_snapshot()::text ~ '^[0-9]+:[0-9]+:'",
     "SELECT txid_current_snapshot()::text ~ '^zzz'"),
    ("age(xid)", "SELECT age('1'::xid) >= 0", "SELECT age('1'::xid) < 0"),
    ("pg_relation_filenode", "SELECT pg_relation_filenode('pg_class'::regclass) IS NOT NULL",
     "SELECT pg_relation_filenode(999999::oid) IS NOT NULL"),
    ("pg_indexes_size", "SELECT pg_indexes_size('pg_class'::regclass) >= 0",
     "SELECT pg_indexes_size('pg_class'::regclass) > 1e15"),
    ("pg_size_pretty", "SELECT pg_size_pretty(1024::bigint)",
     "SELECT pg_size_pretty(1048576::bigint)"),
    ("row_security_active", "SELECT row_security_active('pg_class'::regclass)",
     "SELECT NOT row_security_active('pg_class'::regclass)"),
    ("pg_opclass_is_visible-ish/pg_ts_config_is_visible",
     "SELECT pg_ts_config_is_visible('english'::regconfig)",
     "SELECT pg_ts_config_is_visible(999999::oid)"),
    ("pg_trigger_depth", "SELECT pg_trigger_depth() = 0", "SELECT pg_trigger_depth() > 0"),
    ("statement_timestamp/clock", "SELECT statement_timestamp() <= clock_timestamp()",
     "SELECT statement_timestamp() > clock_timestamp()"),
    ("pg_conf_load_time", "SELECT pg_conf_load_time() IS NOT NULL",
     "SELECT pg_conf_load_time() IS NULL"),
    ("pg_postmaster_start_time", "SELECT pg_postmaster_start_time() IS NOT NULL",
     "SELECT pg_postmaster_start_time() IS NULL"),
    ("current_setting", "SELECT current_setting('search_path') IS NOT NULL",
     "SELECT current_setting('search_path') = 'zzz'"),
    ("pg_get_viewdef", "SELECT pg_get_viewdef(999999::oid) IS NULL",
     "SELECT pg_get_viewdef(999999::oid) IS NOT NULL"),
    ("pg_get_indexdef", "SELECT pg_get_indexdef(999999::oid) IS NULL",
     "SELECT pg_get_indexdef(999999::oid) IS NOT NULL"),
    ("pg_stat_get_numscans", "SELECT pg_stat_get_numscans('pg_class'::regclass) >= 0",
     "SELECT pg_stat_get_numscans('pg_class'::regclass) < 0"),
]


def run(host, port, db, user, password, sql):
    url = f"postgres://{user}@{host}:{port}/{db}?sslmode=disable"
    env = ["-e", f"PGPASSWORD={password}"] if password else []
    r = subprocess.run(
        ["docker", "run", "--rm", "-i", *env, "postgres:18-alpine", "psql", url, "-A", "-t", "-c", sql],
        capture_output=True, text=True, timeout=120,
    )
    if r.returncode != 0:
        return "ERR"
    return (r.stdout or "").strip() or "<null>"


def main():
    pg_host, pg_port, spg_host, spg_port = sys.argv[1:5]
    print(f"{'function':<34} {'PG a|b':<22} SPG a|b")
    suspects = 0
    for label, a, b in PAIRS:
        pa = run(pg_host, pg_port, "bench", "bench", "bench", a)
        pb = run(pg_host, pg_port, "bench", "bench", "bench", b)
        sa = run(spg_host, spg_port, "postgres", "unmei", None, a)
        sb = run(spg_host, spg_port, "postgres", "unmei", None, b)
        # PG must distinguish the pair for the case to mean anything.
        if pa == pb or "ERR" in (pa, pb):
            continue
        if sa == sb:
            suspects += 1
            print(f"{label:<34} {pa}|{pb:<20} {sa}|{sb}   <-- same both ways")
        elif (sa, sb) != (pa, pb):
            print(f"{label:<34} {pa}|{pb:<20} {sa}|{sb}   differs")
    print(f"\n# {suspects} answered the same to both inputs where PG did not")


if __name__ == "__main__":
    main()
