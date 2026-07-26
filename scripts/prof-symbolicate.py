#!/usr/bin/env python3
"""Resolve a --save-only samply profile against `nm -n` output.

samply leaves `nativeSymbols` empty here, so the binary's symbol table is
the only thing that can name a frame. Addresses in the profile are
per-library offsets, so a frame is only resolvable against `nm` if it
belongs to the profiled binary itself — every other library keeps its
lib name. (Resolving a libsystem_malloc offset against the binary's
symbols invents plausible-looking names out of nothing.)

  prof.py <profile.json.gz> <binary>            -> self-time top 30
  prof.py <profile.json.gz> <binary> <needle>   -> callers of frames whose
                                                   name contains <needle>
"""
import gzip, json, os, re, subprocess, sys, collections, bisect

prof_path, binary = sys.argv[1], sys.argv[2]
needle = sys.argv[3] if len(sys.argv) > 3 else None
# v7.39 (round 491) — a server profile has one thread per background worker
# plus the connection thread, and mixing them makes idle sleeps look like
# cost. SPG_PROF_THREAD restricts the aggregate to threads whose name
# contains the value.
thread_filter = os.environ.get("SPG_PROF_THREAD")
BASE = 0x100000000
OWN_LIB = binary.rsplit("/", 1)[-1]

nm = subprocess.run(["nm", "-n", binary], capture_output=True, text=True).stdout
syms = []
for line in nm.splitlines():
    m = re.match(r"^([0-9a-fA-F]+) [tT] (.+)$", line)
    if m:
        syms.append((int(m.group(1), 16), m.group(2)))
syms.sort()
addrs = [s[0] for s in syms]


def resolve(a):
    i = bisect.bisect_right(addrs, a) - 1
    return syms[i][1] if i >= 0 else "??"


with gzip.open(prof_path) as f:
    d = json.load(f)

lib_names = [l["name"] for l in d["libs"]]
cache = {}


def label_of(th, sa, fi):
    key = (id(th), fi)
    if key in cache:
        return cache[key]
    funcs = th["funcTable"]
    fu = th["frameTable"]["func"][fi]
    name = sa[funcs["name"][fu]]
    res = funcs["resource"][fu]
    lib = None
    if res is not None and res >= 0:
        lib = lib_names[th["resourceTable"]["lib"][res]]
    if name.startswith("0x"):
        if lib == OWN_LIB:
            name = resolve(int(name, 16) + BASE)
        else:
            name = f"[{lib or 'unknown'}] {name}"
    cache[key] = name
    return name


self_counts = collections.Counter()
caller_counts = collections.Counter()
total = 0
for th in d["threads"]:
    if thread_filter and thread_filter not in (th.get("name") or ""):
        continue
    sa = th["stringArray"]
    st_frame = th["stackTable"]["frame"]
    st_prefix = th["stackTable"]["prefix"]
    for si in th["samples"]["stack"]:
        if si is None:
            continue
        total += 1
        leaf = label_of(th, sa, st_frame[si])
        self_counts[leaf] += 1
        if needle and needle in leaf:
            p, chain = st_prefix[si], []
            while p is not None and len(chain) < 6:
                chain.append(label_of(th, sa, st_frame[p]))
                p = st_prefix[p]
            caller_counts[" <- ".join(chain)] += 1

out = [f"total samples: {total}"]
for name, c in (caller_counts if needle else self_counts).most_common(30):
    out.append(f"{100.0 * c / total:6.2f}%  {c:5d}  {name}")
text = "\n".join(out)
p = subprocess.run(["rustfilt"], input=text, capture_output=True, text=True)
print(p.stdout if p.returncode == 0 else text)
