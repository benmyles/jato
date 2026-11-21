#!/usr/bin/env python3
"""Time one real Cargo publication against a synthetic pending dependency graph."""
import argparse
import hashlib
import json
import os
import platform
import re
import shutil
import sqlite3
import statistics
import subprocess
import tempfile
import time
from contextlib import closing
from pathlib import Path

from compare import binary, run
from worktrees import cargo_environment


def symbol(i):
    return i.to_bytes(32, "little")


def seed(db, unrelated, depth, producer):
    # These rows model dependencies only. Cargo builds and checks the real
    # producer. No synthetic entry is ever selected as a compiler result.
    count = unrelated + depth
    db.executemany("INSERT INTO entries VALUES(?,?,?,0,x'',zeroblob(32),0)",
                   ((i, symbol(i), symbol(0)) for i in range(1, count + 1)))
    db.executemany("INSERT INTO plans(entry,inputs,topology,codegen) VALUES(?,x'7b7d',zeroblob(32),0)",
                   ((i,) for i in range(1, count + 1)))
    db.executemany("INSERT INTO files VALUES(?,0,?,32,420,0)",
                   ((i, symbol(i)) for i in range(1, count + 1)))
    db.executemany("INSERT INTO needs VALUES(?,?,0)",
                   ((i, symbol(count + i)) for i in range(1, unrelated + 1)))
    # Descendants finish first, then a real producer supplies the missing root.
    db.executemany("INSERT INTO needs VALUES(?,?,0)", (
        (i, producer if i == count else symbol(i + 1))
        for i in range(unrelated + 1, count + 1)
    ))
    db.commit()
    db.execute("PRAGMA wal_checkpoint(TRUNCATE)")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jato", default="jato")
    parser.add_argument("--previous-jato", required=True)
    parser.add_argument("--pending", type=int, default=16384)
    parser.add_argument("--depth", type=int, default=64)
    parser.add_argument("--runs", type=int, default=7)
    parser.add_argument("--scratch", default=os.getenv("TMPDIR") or "/var/tmp")
    opts = parser.parse_args()
    if min(opts.runs, opts.depth, opts.pending) < 1:
        parser.error("counts must be positive")
    tools = {"jato": binary(opts.jato), "previous": binary(opts.previous_jato)}
    rustc = binary(str(Path(subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip()) / "bin/rustc"))
    cases = {"empty": (0, 0), "unrelated": (opts.pending, 0), "chain": (opts.pending, opts.depth)}
    with tempfile.TemporaryDirectory(dir=opts.scratch) as scratch:
        root = Path(scratch)
        project = root / "project"
        target = root / "target"
        (project / "src").mkdir(parents=True)
        (project / "Cargo.toml").write_text('[package]\nname="publisher"\nversion="0.1.0"\nedition="2021"\n')
        (project / "src/lib.rs").write_text("pub const VALUE: usize = 42;\n")
        check = root / "verify.rs"
        check.write_text("const _: [(); 42] = [(); publisher::VALUE];\n")
        env = dict(cargo_environment(root, rustc, 1), CARGO_TARGET_DIR=str(target),
                   CARGO_BUILD_BUILD_DIR=str(target), JATO_LOG="timings", RUSTC_WRAPPER="")
        run([env["CARGO"], "generate-lockfile", "--offline"], env, project)
        versions = {}
        rows = {name: {case: [] for case in cases} for name in tools}
        dbname = "artifacts-v1.sqlite3"
        expected = None
        for name, exe in tools.items():
            store = root / name
            cfg = dict(env, JATO_CACHE_DIR=str(store))
            versions[name] = run([exe, "--version"], cfg, project).decode().strip()
            run([exe, "check", "--locked", "-q"], cfg, project)
            with closing(sqlite3.connect(store / dbname)) as db:
                producer, = db.execute("SELECT digest FROM files WHERE slot=1").fetchone()
            rmeta, = target.rglob("libpublisher-*.rmeta")
            digest = hashlib.sha256(rmeta.read_bytes()).digest()
            assert expected is None or digest == expected
            expected = digest
            run([exe, "gc", "0"], cfg, project)
            shutil.rmtree(target)
            for case, (unrelated, depth) in cases.items():
                template = root / f"{name}-{case}.sqlite"
                with closing(sqlite3.connect(store / dbname)) as src, closing(sqlite3.connect(template)) as db:
                    src.backup(db)
                    seed(db, unrelated, depth, producer)
        for i in range(opts.runs):
            for case, (unrelated, _) in cases.items():
                for name in (list(tools) if i % 2 == 0 else reversed(tools)):
                    with tempfile.TemporaryDirectory(dir=root) as store:
                        dest = Path(store) / dbname
                        shutil.copyfile(root / f"{name}-{case}.sqlite", dest)
                        cfg = dict(env, JATO_CACHE_DIR=store)
                        start = time.perf_counter()
                        result = subprocess.run([tools[name], "check", "--locked", "-q"], env=cfg,
                                                cwd=project, capture_output=True, check=True)
                        elapsed = time.perf_counter() - start
                        us = re.search(rb"jato \[publisher\].*?store (\d+) us", result.stderr)
                        assert us, result.stderr
                        rows[name][case].append(dict(seconds=round(elapsed, 6), store_us=int(us[1])))
                        rmeta, = target.rglob("libpublisher-*.rmeta")
                        assert hashlib.sha256(rmeta.read_bytes()).digest() == expected
                        run([rustc, "--crate-type=lib", "--emit=metadata", "--extern", f"publisher={rmeta}",
                             check, "-o", root / "verified.rmeta"], cfg, project)
                        with closing(sqlite3.connect(dest)) as db:
                            assert db.execute("SELECT COUNT(*) FROM needs").fetchone() == (unrelated,)
                        shutil.rmtree(target)
        medians = {name: {case: {key: statistics.median(r[key] for r in group) for key in group[0]}
                          for case, group in samples.items()} for name, samples in rows.items()}
        print(json.dumps(dict(versions=versions, kernel=platform.release(), rustc=run([rustc, "-V"], env, project).decode().strip(),
                              pending=opts.pending, depth=opts.depth, runs=opts.runs, jobs=1,
                              samples=rows, median=medians), indent=2))


if __name__ == "__main__":
    main()
