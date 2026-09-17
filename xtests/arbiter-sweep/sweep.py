# ON CONFLICT arbitration, every shape against PostgreSQL.
#
#   sweep.py <pg-uri> <spg-uri>        exit 0 = no case differs, 1 = one does
#
# 8.0.3 — a release gate (`write-arbitration`). The axes it varies: arbiter
# column type, arity, constraint vs unique index, full vs PARTIAL index,
# target spelling, DO NOTHING vs DO UPDATE, and an update arm that collides
# with ANOTHER unique key. What it holds still: one session, autocommit —
# the held-transaction panel beside it covers a second session holding
# the row.
#
# 9.0.0 — and whether a key part is an EXPRESSION: none, the leading part,
# or (composite) the second part. The expression is `lower(k)` over text,
# `(k + 0)` over int, and the incoming row spells a text key in upper case,
# so an arbiter that reads the column instead of the expression misses the
# conflict. Only a unique index can key on an expression.
import itertools, os, subprocess, sys, json, re
PG, SPG = sys.argv[1], sys.argv[2]
PSQL = os.environ.get("PSQL", "psql")
types = {"int": ("int", ["1","2"]), "text": ("text", ["'a'","'b'"]), "textC": ('text COLLATE "C"', ["'a'","'b'"])}
cases = []
n = 0
for ty, arity, source, partial, target, action, second, keyexpr in itertools.product(
        types, ["single","composite"], ["pk","unique","uindex"], [False, True],
        ["named","untargeted","constraint"], ["nothing","update"], [False, True],
        ["none","lead","second"]):
    if keyexpr != "none" and source != "uindex": continue       # only an index keys on an expression
    if keyexpr == "second" and arity == "single": continue      # there is no second part
    if source in ("pk","unique") and partial: continue          # constraints cannot be partial
    if target == "constraint" and source == "uindex": continue   # ON CONSTRAINT names a constraint
    if second and action != "update": continue                   # the update arm's other key
    if target == "untargeted" and action == "update": continue   # PG requires a target (42601) — covered separately
    n += 1
    t = f"s{n}"
    coltype, vals = types[ty]
    cols = ["k1"] if arity == "single" else ["k1","k2"]
    ddl = [f"CREATE TABLE {t} (id serial, k1 {coltype}, k2 {coltype}, flag int, other int, v int)"]
    def part(c, i):
        on = (keyexpr == "lead" and i == 0) or (keyexpr == "second" and i == 1)
        if not on: return c
        return f"(({c} + 0))" if ty == "int" else f"lower({c})"
    keycols = ", ".join(part(c, i) for i, c in enumerate(cols))
    incoming = vals[0] if (keyexpr == "none" or ty == "int") else vals[0].upper()
    pred = " WHERE flag IS NOT NULL" if partial else ""
    if source == "pk":     ddl[0] = ddl[0][:-1] + f", PRIMARY KEY ({keycols}))"
    if source == "unique": ddl[0] = ddl[0][:-1] + f", CONSTRAINT {t}_uq UNIQUE ({keycols}))"
    if source == "uindex": ddl.append(f"CREATE UNIQUE INDEX {t}_ux ON {t} ({keycols}){pred}")
    if second: ddl.append(f"CREATE UNIQUE INDEX {t}_other ON {t} (other)")
    ddl.append(f"INSERT INTO {t} (k1, k2, flag, other, v) VALUES ({vals[0]}, {vals[0]}, 1, 100, 1)")
    ddl.append(f"INSERT INTO {t} (k1, k2, flag, other, v) VALUES ({vals[1]}, {vals[1]}, 1, 200, 1)")
    if target == "named": tgt = f"({keycols}){pred}"
    elif target == "untargeted": tgt = ""
    else:
        conname = f"{t}_pkey" if source == "pk" else f"{t}_uq"
        tgt = f"ON CONSTRAINT {conname}"
    act = "DO NOTHING" if action == "nothing" else ("DO UPDATE SET other = 200" if second else "DO UPDATE SET v = EXCLUDED.v")
    stmt = f"INSERT INTO {t} (k1, k2, flag, other, v) VALUES ({incoming}, {incoming}, 1, 300, 2) ON CONFLICT {tgt} {act}"
    check = f"SELECT string_agg(k1::text||'/'||k2::text||'/'||coalesce(other::text,'-')||'/'||v::text, ',' ORDER BY id) FROM {t}"
    name = f"{ty}|{arity}|{source}|{'partial' if partial else 'full'}|{target}|{action}{'+otherkey' if second else ''}|expr={keyexpr}"
    cases.append((name, t, ddl, stmt, check))

def run(uri, t, ddl, stmt, check):
    sql = f"DROP TABLE IF EXISTS {t};\n" + ";\n".join(ddl) + ";\n" + "\\echo ---STMT\n" + stmt + ";\n\\echo ---ROWS\n" + check + ";\n"
    r = subprocess.run([PSQL, uri, "--no-psqlrc", "-X", "-tA", "-v", "VERBOSITY=terse"], input=sql, capture_output=True, text=True, timeout=60)
    out = r.stdout + r.stderr
    part = out.split("---STMT",1)[1] if "---STMT" in out else "SETUP:" + out
    part = re.sub(r"psql:<stdin>:\d+: ", "", part)
    return " ".join(l.strip() for l in part.splitlines() if l.strip() and not l.startswith("NOTICE"))

diffs = 0
for name, t, ddl, stmt, check in cases:
    a = run(PG, t, ddl, stmt, check); b = run(SPG, t, ddl, stmt, check)
    if a != b:
        diffs += 1
        print(f"DIFF {name}\n   PG : {a}\n   SPG: {b}")
print(f"cases={len(cases)} diffs={diffs}")
# A sweep that ran nothing is not a green one.
if len(cases) < 200:
    print(f"only {len(cases)} cases built; the sweep is broken")
    sys.exit(2)
sys.exit(1 if diffs else 0)
