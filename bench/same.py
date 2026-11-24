#!/usr/bin/env python3
# same.py A B : compare two target directories file by file, with the location of
# each directory and of its checkout taken out of what the files say.
import collections
import hashlib
import os
import sys


def scan(top):
    top = os.path.realpath(top)
    work = os.path.dirname(top)
    out = {}
    for d, dirs, files in os.walk(top):
        if "incremental" in dirs:
            dirs.remove("incremental")
        for f in files:
            if f.startswith(".jato-") or f == ".rustc_info.json":
                continue
            p = os.path.join(d, f)
            try:
                data = open(p, "rb").read()
            except OSError:
                continue
            data = data.replace(top.encode(), b"<T>").replace(work.encode(), b"<W>")
            out[os.path.relpath(p, top)] = hashlib.blake2b(data, digest_size=16).digest()
    return out


def unit(p):
    parts = p.split(os.sep)
    return "/".join(parts[:4]) if len(parts) > 4 else "/".join(parts[:-1])


a, b = scan(sys.argv[1]), scan(sys.argv[2])
only_a = sorted(set(a) - set(b))
only_b = sorted(set(b) - set(a))
differ = sorted(k for k in a.keys() & b.keys() if a[k] != b[k])
print(f"{len(a)} and {len(b)} files; the same: {len(a.keys() & b.keys()) - len(differ)}")
print(f"only in the first: {len(only_a)}; only in the second: {len(only_b)}; holding something else: {len(differ)}")
for name, group in (("only in the first", only_a), ("only in the second", only_b), ("differing", differ)):
    c = collections.Counter(unit(p) for p in group)
    for u, n in c.most_common(14):
        print(f"   {name}: {n:4} {u}")
    kinds = collections.Counter(os.path.splitext(p)[1] or os.path.basename(p) for p in group)
    if group:
        print(f"   {name}, by kind: {dict(kinds.most_common(8))}")
