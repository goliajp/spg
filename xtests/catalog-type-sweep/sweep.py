# The type every `pg_*` catalog column is DECLARED as, SPG against PostgreSQL.
#
#   sweep.py <pg-uri> <spg-uri>    exit 0 = only recorded divergences,
#                                  1 = a new one, 2 = the sweep could not run
#
# 9.0.0 — a driver decodes a column by the type it is announced as, so a
# catalog column typed `bigint` where PostgreSQL says `oid` is a wrong
# decode in binary, silently. 142 such columns were found by asking
# PostgreSQL rather than by guessing names; the 122 it calls `oid` are
# closed, and what is left is recorded below.
#
# Three ways this instrument can fail, and each is made to FAIL LOUDLY
# rather than look like agreement:
#   * a relation that cannot materialise — a declared type no row carries
#     fails the whole relation, and a sweep that reads pg_attribute alone
#     never learns pg_constraint is broken. SELECT * over every catalog
#     relation first, after creating a table, a view, a composite type and
#     an index: an EMPTY database reaches none of the per-kind row
#     builders, and the first version of this sweep passed while the ones
#     for a view and a composite type still held the old width.
#   * an empty or short answer from either side — a floor on the count.
#   * a comparison over a handful of columns — a floor on the overlap.
import subprocess, sys, os, collections

RECORDED = {
    "pg_am.amhandler",
    "pg_amproc.amproc",
    "pg_attrdef.adbin",
    "pg_attribute.attacl",
    "pg_attribute.attfdwoptions",
    "pg_attribute.attmissingval",
    "pg_attribute.attoptions",
    "pg_class.relacl",
    "pg_class.relminmxid",
    "pg_class.relpartbound",
    "pg_class.reltuples",
    "pg_constraint.conbin",
    "pg_constraint.conexclop",
    "pg_constraint.confdelsetcols",
    "pg_constraint.conffeqop",
    "pg_constraint.conpfeqop",
    "pg_constraint.conppeqop",
    "pg_enum.enumsortorder",
    "pg_extension.extconfig",
    "pg_index.indexprs",
    "pg_index.indpred",
    "pg_largeobject_metadata.lomacl",
    "pg_namespace.nspacl",
    "pg_operator.oprcode",
    "pg_operator.oprjoin",
    "pg_operator.oprrest",
    "pg_policy.polqual",
    "pg_policy.polroles",
    "pg_policy.polwithcheck",
    "pg_proc.proacl",
    "pg_proc.proallargtypes",
    "pg_proc.proargdefaults",
    "pg_proc.proargmodes",
    "pg_proc.procost",
    "pg_proc.prorows",
    "pg_proc.prosqlbody",
    "pg_proc.prosupport",
    "pg_proc.protrftypes",
    "pg_statistic.stadistinct",
    "pg_statistic.stanullfrac",
    "pg_statistic_ext.stxexprs",
    "pg_statistic_ext.stxkeys",
    "pg_statistic_ext.stxkind",
    "pg_stats.correlation",
    "pg_stats.elem_count_histogram",
    "pg_stats.histogram_bounds",
    "pg_stats.most_common_elem_freqs",
    "pg_stats.most_common_elems",
    "pg_stats.most_common_freqs",
    "pg_stats.most_common_vals",
    "pg_stats.n_distinct",
    "pg_stats.null_frac",
    "pg_stats.range_bounds_histogram",
    "pg_stats.range_empty_frac",
    "pg_stats.range_length_histogram",
    "pg_tablespace.spcacl",
    "pg_tablespace.spcoptions",
    "pg_trigger.tgargs",
    "pg_trigger.tgattr",
    "pg_trigger.tgqual",
    "pg_ts_parser.prsend",
    "pg_ts_parser.prsheadline",
    "pg_ts_parser.prslextype",
    "pg_ts_parser.prsstart",
    "pg_ts_parser.prstoken",
    "pg_ts_template.tmplinit",
    "pg_ts_template.tmpllexize",
    "pg_type.typacl",
    "pg_type.typanalyze",
    "pg_type.typdefaultbin",
    "pg_type.typinput",
    "pg_type.typmodin",
    "pg_type.typmodout",
    "pg_type.typoutput",
    "pg_type.typreceive",
    "pg_type.typsend",
    "pg_type.typsubscript",
}

TYPES = ("SELECT c.relname||'.'||a.attname||'='||format_type(a.atttypid,-1) "
         "FROM pg_class c JOIN pg_attribute a ON a.attrelid=c.oid "
         "WHERE c.relname LIKE 'pg\\_%' AND a.attnum>0")
PSQL = os.environ.get("PSQL", "psql")


def psql(uri, sql):
    return subprocess.run([PSQL, uri, "-X", "-tAc", sql], capture_output=True, text=True)


def select_star_every_catalog(uri):
    for ddl in ("DROP VIEW IF EXISTS sweep_v",
                "DROP TABLE IF EXISTS sweep_t",
                "DROP TYPE IF EXISTS sweep_c",
                "CREATE TABLE sweep_t(a int, b text)",
                "CREATE VIEW sweep_v AS SELECT a, b FROM sweep_t",
                "CREATE TYPE sweep_c AS (x int, y text)",
                "CREATE INDEX sweep_i ON sweep_t(a)",
                "DROP FUNCTION IF EXISTS sweep_f(int)",
                "CREATE FUNCTION sweep_f(x int) RETURNS int LANGUAGE sql AS 'SELECT x'"):
        psql(uri, ddl)
    r = psql(uri, "SELECT relname FROM pg_class WHERE relname LIKE 'pg\\_%' ORDER BY 1")
    names = r.stdout.split()
    if r.returncode != 0 or len(names) < 20:
        sys.exit(f"the sweep could not list the catalogs: {len(names)} found, "
                 f"{r.stderr.strip()[:150]}")
    bad = []
    for n in names:
        q = psql(uri, f"SELECT * FROM {n} LIMIT 1")
        if q.returncode != 0:
            bad.append(f"{n}: {q.stderr.strip().splitlines()[0][:90]}")
    if bad:
        sys.exit("catalog relations that do not materialise:\n  " + "\n  ".join(bad))
    print(f"select * over {len(names)} catalog relations: ok")


def types(uri, extra=""):
    r = psql(uri, TYPES + extra + " ORDER BY 1")
    rows = dict(l.strip().rsplit("=", 1) for l in r.stdout.splitlines() if "=" in l)
    if r.returncode != 0 or len(rows) < 200:
        sys.exit(f"the sweep could not run on {uri}: {len(rows)} columns, "
                 f"{r.stderr.strip()[:150]}")
    return rows


PG, SPG = sys.argv[1], sys.argv[2]
select_star_every_catalog(SPG)
spg = types(SPG)
want = " AND (c.relname||'.'||a.attname) IN (" + ",".join("'%s'" % k for k in spg) + ")"
pg = types(PG, want)
common = [k for k in sorted(pg) if k in spg]
if len(common) < 200:
    sys.exit(f"only {len(common)} columns in common - not a sweep")
new = 0
shape = collections.Counter()
for k in common:
    if spg[k] == pg[k]:
        continue
    shape[(spg[k], pg[k])] += 1
    if k not in RECORDED:
        new += 1
        print(f"NEW      {k:42s} SPG={spg[k]:22s} PG={pg[k]}")
for (a, b), n in shape.most_common():
    print(f"recorded {n:4d}  SPG={a} PG={b}")
print(f"columns={len(common)} recorded={len(RECORDED)} new={new}")
sys.exit(1 if new else 0)
