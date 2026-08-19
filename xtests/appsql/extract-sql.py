#!/usr/bin/env python3
"""Pull every SQL literal out of an application's Rust sources.

APP_ROOT points at the application checkout (default: the cwd).

They pass SQL to sqlx::query* as a string literal, usually a raw
multi-line one. Take the literal that follows each call, keep the ones
that actually look like SQL, and emit one JSON record per site so a
prober can Describe each against a real schema.
"""
import json, os, re, sys, pathlib

ROOT = pathlib.Path(os.environ.get("APP_ROOT", "."))
CALL = re.compile(r'sqlx::query(?:_as|_scalar|_with|_as_with)?(?:::<[^>]*>)?\s*\(')

def read_literal(text, i):
    """At i (just past the call's open paren) read the next string literal."""
    n = len(text)
    while i < n and text[i] in ' \t\r\n':
        i += 1
    # raw string: r"..." or r#"..."#
    if text.startswith('r', i):
        j = i + 1
        hashes = 0
        while j < n and text[j] == '#':
            hashes += 1; j += 1
        if j < n and text[j] == '"':
            close = '"' + '#' * hashes
            end = text.find(close, j + 1)
            if end == -1: return None, i
            return text[j+1:end], end + len(close)
    if i < n and text[i] == '"':
        out = []; j = i + 1
        while j < n:
            c = text[j]
            if c == '\\': out.append(text[j+1] if j+1 < n else ''); j += 2; continue
            if c == '"': return ''.join(out), j + 1
            out.append(c); j += 1
    return None, i

SQL_START = re.compile(r'^\s*(SELECT|INSERT|UPDATE|DELETE|WITH|MERGE)\b', re.I)

records = []
for path in ROOT.rglob("*.rs"):
    if "/target/" in str(path) or "/node_modules/" in str(path):
        continue
    try: text = path.read_text()
    except Exception: continue
    for m in CALL.finditer(text):
        lit, _ = read_literal(text, m.end())
        if not lit: continue
        sql = lit.strip()
        if not SQL_START.match(sql): continue
        line = text.count('\n', 0, m.start()) + 1
        records.append({
            "file": str(path.relative_to(ROOT)),
            "line": line,
            "sql": sql,
        })
seen = set(); uniq = []
for r in records:
    k = " ".join(r["sql"].split())
    if k in seen: continue
    seen.add(k); uniq.append(r)
json.dump(uniq, sys.stdout, indent=1)
print(f"\n-- {len(records)} sites, {len(uniq)} distinct statements", file=sys.stderr)
