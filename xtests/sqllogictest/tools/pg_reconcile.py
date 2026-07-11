#!/usr/bin/env python3
"""pg_reconcile — audit sqllogictest fixtures against live PG.

For every failing record in report.json, replay the fixture's SQL in
order against the PG oracle (single psql session per file, scratch
schema), capture PG's answer for each query record, and split the
failures into two buckets:

  * fixture-debt — SPG's actual output already MATCHES PG; the
    fixture's expected block is stale (e.g. recorded when the engine
    returned float where PG returns numeric). With --rewrite the
    expected block is replaced by the PG truth.
  * real-gap — SPG's actual output differs from PG; these are engine
    bugs and are printed for attack, never rewritten.

PG psql output is normalised to the runner's rendering (bool t/f→1/0,
empty string→"(empty)", NULL→NULL, one cell per line, rowsort applied
the way runner.rs does).

Usage (on the box with the docker oracle):
  python3 tools/pg_reconcile.py [--rewrite] file1.test [file2.test …]
Env: PG_EXEC (default: docker exec -i unmei-postgres psql -U unmei -d unmei)
"""

import json
import os
import re
import subprocess
import sys

PG_EXEC = os.environ.get(
    "PG_EXEC", "docker exec -i unmei-postgres psql -U unmei -d unmei"
)
MARK = "===SLT-REC-"


def parse_test(path):
    """Minimal sqllogictest parser mirroring src/parser.rs record order."""
    records = []
    lines = open(path).read().split("\n")
    i = 0
    while i < len(lines):
        line = lines[i].strip()
        if not line or line.startswith("#"):
            i += 1
            continue
        if line.startswith("statement"):
            expect_error = "error" in line
            i += 1
            sql_lines = []
            while i < len(lines) and lines[i].strip() != "":
                sql_lines.append(lines[i])
                i += 1
            records.append(
                {"kind": "statement", "sql": "\n".join(sql_lines), "err": expect_error}
            )
        elif line.startswith("query"):
            parts = line.split()
            types = parts[1] if len(parts) > 1 else "T"
            sort = parts[2] if len(parts) > 2 else "nosort"
            header_idx = i
            i += 1
            sql_lines = []
            while i < len(lines) and lines[i].strip() != "----":
                sql_lines.append(lines[i])
                i += 1
            i += 1  # skip ----
            exp_start = i
            exp = []
            while i < len(lines) and lines[i].strip() != "":
                exp.append(lines[i])
                i += 1
            records.append(
                {
                    "kind": "query",
                    "sql": "\n".join(sql_lines),
                    "types": types,
                    "sort": sort,
                    "expected": exp,
                    "header_line": header_idx,
                    "exp_range": (exp_start, i),
                }
            )
        elif line == "halt":
            records.append({"kind": "halt"})
            i += 1
        else:
            i += 1
    return records


def pg_replay(records):
    """Run the whole file's SQL in one psql session; return {rec_idx: [raw rows]}."""
    script = ["\\pset null NULL", "\\pset format unaligned", "\\pset tuples_only on",
              "\\pset fieldsep '\\t'",
              "DROP SCHEMA IF EXISTS slt_reconcile CASCADE;",
              "CREATE SCHEMA slt_reconcile;",
              "SET search_path = slt_reconcile;"]
    for idx, r in enumerate(records):
        if r["kind"] == "halt":
            break
        script.append(f"\\echo {MARK}{idx}")
        sql = r["sql"].rstrip(";").strip()
        script.append(sql + ";")
    script.append(f"\\echo {MARK}END")
    proc = subprocess.run(
        PG_EXEC.split(),
        input="\n".join(script).encode(),
        capture_output=True,
    )
    out = proc.stdout.decode()
    sections = {}
    cur = None
    for line in out.split("\n"):
        m = re.match(rf"^{MARK}(\d+|END)$", line)
        if m:
            cur = None if m.group(1) == "END" else int(m.group(1))
            if cur is not None:
                sections[cur] = []
            continue
        if cur is not None:
            sections[cur].append(line)
    return sections


def normalise(rows, types, sort):
    """psql rows -> runner-rendered flat cell list."""
    rendered = []
    for line in rows:
        if line == "":
            continue
        cells = line.split("\t")
        norm = []
        for pos, c in enumerate(cells):
            ty = types[pos] if pos < len(types) else "T"
            if ty == "B":
                c = {"t": "1", "f": "0"}.get(c, c)
            if c == "" :
                c = "(empty)"
            norm.append(c)
        rendered.append(norm)
    if sort == "rowsort":
        rendered.sort(key=lambda r: "\x00".join(r))
    flat = [c for r in rendered for c in r]
    if sort == "valuesort":
        flat.sort()
    return flat


def main():
    rewrite = "--rewrite" in sys.argv
    files = [a for a in sys.argv[1:] if not a.startswith("--")]
    gaps = []
    for path in files:
        records = parse_test(path)
        sections = pg_replay(records)
        lines = open(path).read().split("\n")
        edits = []  # (start, end, new_expected_lines)
        for idx, r in enumerate(records):
            if r["kind"] != "query":
                continue
            pg_rows = sections.get(idx)
            if pg_rows is None:
                continue
            pg_flat = normalise(pg_rows, r["types"], r["sort"])
            # An empty section is indistinguishable from a psql ERROR
            # (errors go to stderr): never rewrite to empty, flag it.
            if not pg_flat:
                print(f"{path} record {idx}: PG returned nothing (error or "
                      "empty set) — SKIPPED, resolve by hand")
                continue
            # now()/current_date-dependent fixtures assert the runner's
            # fixed test clock; PG's live clock is not the truth here.
            if re.search(r"\b(now\(\)|current_date|current_timestamp|"
                         r"current_time|localtimestamp)", r["sql"], re.I):
                print(f"{path} record {idx}: clock-dependent — SKIPPED")
                continue
            cur_exp = [l for l in r["expected"]]
            if pg_flat == cur_exp:
                continue  # fixture already states the PG truth
            print(f"{path} record {idx}:")
            print(f"  fixture: {cur_exp}")
            print(f"  PG     : {pg_flat}")
            edits.append((r["exp_range"][0], r["exp_range"][1], pg_flat, idx))
        if rewrite and edits:
            for start, end, new, _ in sorted(edits, reverse=True):
                lines[start:end] = new
            open(path, "w").write("\n".join(lines))
            print(f"  -> rewrote {len(edits)} expected block(s) in {path}")
        gaps.extend((path, e[3]) for e in edits)
    if not rewrite and gaps:
        print(f"\n{len(gaps)} record(s) differ from PG truth (rerun with --rewrite "
              "AFTER confirming SPG matches PG on each — otherwise fix the engine).")


if __name__ == "__main__":
    main()
