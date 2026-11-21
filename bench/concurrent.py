#!/usr/bin/env python3
"""Measure simultaneous cold Cargo builds in independent Linux worktrees."""

import argparse
import ctypes
import json
import multiprocessing
import os
from pathlib import Path
import platform
import resource
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time

from compare import binary, run, tree_bytes
from worktrees import cargo_environment, private_environment, counters


def descendants(root):
    """Follow only this trial's processes, including children of worker threads."""
    pending = [root]
    seen = set()
    while pending:
        pid = pending.pop()
        if pid in seen:
            continue
        seen.add(pid)
        for children in Path(f"/proc/{pid}/task").glob("*/children"):
            try:
                pending.extend(map(int, children.read_text().split()))
            except (FileNotFoundError, ProcessLookupError):
                pass
    return seen - {root}


def sample_memory(finished, peak):
    while not finished.is_set():
        total = 0
        for pid in descendants(os.getpid()):
            try:
                with Path(f"/proc/{pid}/smaps_rollup").open() as stream:
                    total += next(
                        int(line.split()[1])
                        for line in stream
                        if line.startswith("Pss:")
                    )
            except (
                FileNotFoundError,
                ProcessLookupError,
                PermissionError,
                StopIteration,
            ):
                pass
        peak[0] = max(total, peak[0])
        finished.wait(0.2)


def reap_servers():
    deadline = time.monotonic() + 10
    while True:
        try:
            pid, _ = os.waitpid(-1, os.WNOHANG)
        except ChildProcessError:
            return
        if pid == 0:
            if time.monotonic() >= deadline:
                raise RuntimeError("private daemon did not finish shutting down")
            time.sleep(0.01)


def trial(name, exe, cargo, base, projects, targets, private, verify_bin):
    # Adopt only children of this trial. After stopping its daemon, wait4's
    # child resource accounting includes daemon-launched rustc processes.
    if (
        ctypes.CDLL(None, use_errno=True).prctl(36, 1, 0, 0, 0) != 0
    ):  # PR_SET_CHILD_SUBREAPER
        raise OSError(ctypes.get_errno(), "cannot account for private peer servers")
    cache = private / "store"
    config = private / "empty.toml"
    config.touch()
    jato = name in {"jato", "previous_jato"}
    env = private_environment(
        base, private, config, "" if jato or name == "cargo" else str(exe)
    )
    env["SCCACHE_BASEDIRS"] = os.pathsep.join(map(str, [*projects, *targets]))
    server = {
        "sccache": (["--start-server"], ["--stop-server"]),
        "kache": (["daemon", "start"], ["daemon", "stop"]),
    }.get(name)
    if server:
        run([exe, *server[0]], env, projects[0])
    commands = ([exe, "worktree", "--root", "."] if jato else [cargo]) + [
        "build",
        "--release",
        "--locked",
        "-q",
    ]
    jobs, peak = [], [0]
    done = threading.Event()
    sampler = threading.Thread(target=sample_memory, args=(done, peak))
    try:
        start = time.perf_counter()
        for i, (project, target) in enumerate(zip(projects, targets)):
            cfg = dict(
                env, CARGO_TARGET_DIR=str(target), CARGO_BUILD_BUILD_DIR=str(target)
            )
            with (private / f"build-{i}.log").open("wb") as log:
                jobs.append(
                    subprocess.Popen(
                        commands, cwd=project, env=cfg, stdout=log, stderr=log
                    )
                )
        sampler.start()
        codes = [c.wait() for c in jobs]
        elapsed = time.perf_counter() - start
        done.set()
        sampler.join()
        for i, code in enumerate(codes):
            if code:
                raise RuntimeError((private / f"build-{i}.log").read_text())
        expected = None
        for project, target in zip(projects, targets):
            cfg = dict(
                env, CARGO_TARGET_DIR=str(target), CARGO_BUILD_BUILD_DIR=str(target)
            )
            if jato:
                paths = json.loads(
                    run([exe, "worktree", "--root", ".", "paths"], cfg, project)
                )
                target = Path(paths["target_directory"])
            result = run(
                [
                    target / "release" / (verify_bin or "jato-regex-bench"),
                    *(["--version"] if verify_bin else []),
                ],
                cfg,
                project,
            )
            if expected is None:
                expected = result
            assert result == expected and (verify_bin or result == b"3\n"), (
                "incorrect executable"
            )
        row = dict(
            seconds=round(elapsed, 6),
            peak_sampled_tree_pss_kib=peak[0],
            hits=counters("worktree" if jato else name, exe, env, projects[0]).get(
                "hits"
            ),
        )
        if jato:
            stats = json.loads(run([exe, "stats", "--json"], env, projects[0]))
            row.update(
                compiler_misses=stats["counters"].get("miss", 0),
                coalesced_hits=stats["counters"].get("coalesced_hit", 0),
                entries=stats["entries"],
            )
    finally:
        done.set()
        if sampler.ident is not None:
            sampler.join()
        for child in jobs:
            if child.poll() is None:
                child.terminate()
                child.wait()
        if server:
            run([exe, *server[1]], env, projects[0])
        reap_servers()
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    row.update(
        cpu_seconds=round(usage.ru_utime + usage.ru_stime, 6),
        output_blocks=usage.ru_oublock,
        store_bytes=tree_bytes(cache),
    )
    (private / "sample.json").write_text(json.dumps(row))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ["jato", "kache", "sccache"]:
        parser.add_argument("--" + name, default=name)
    parser.add_argument("--previous-jato")
    parser.add_argument(
        "--tools",
        nargs="+",
        choices=["jato", "kache", "sccache", "cargo"],
        default=["jato", "kache", "sccache"],
    )
    parser.add_argument("--project", type=Path)
    parser.add_argument("--verify-bin")
    parser.add_argument("--copies", type=int, default=2)
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--scratch", default=os.environ.get("TMPDIR") or "/var/tmp")
    opts = parser.parse_args()
    if (
        opts.copies < 2
        or min(opts.jobs, opts.runs) < 1
        or (opts.project and not opts.verify_bin)
    ):
        parser.error(
            "use at least two copies, positive jobs and runs, and --verify-bin with --project"
        )
    rustc = binary(
        str(
            Path(
                subprocess.check_output(
                    ["rustc", "--print", "sysroot"], text=True
                ).strip()
            )
            / "bin/rustc"
        )
    )
    cargo = rustc.with_name("cargo")
    tools = {n: cargo if n == "cargo" else binary(getattr(opts, n)) for n in opts.tools}
    if opts.previous_jato:
        tools["previous_jato"] = binary(opts.previous_jato)
    versions = {
        n: subprocess.check_output([p, "--version"], text=True).strip()
        for n, p in tools.items()
    }
    with tempfile.TemporaryDirectory(dir=opts.scratch, prefix="jf-") as scratch:
        root = Path(scratch).resolve()
        original = (opts.project or Path(__file__).parent / "regex-app").resolve()

        def ignore(directory, names):
            ignored = {".git", "__pycache__"}
            if Path(directory) == original:
                ignored.add("target")
            return ignored.intersection(names)

        projects = [root / f"source-{i}" for i in range(opts.copies)]
        targets = [root / f"target-{i}" for i in range(opts.copies)]
        for p in projects:
            shutil.copytree(original, p, symlinks=True, ignore=ignore)
            if not opts.project:
                (p / "Cargo.toml.in").rename(p / "Cargo.toml")
        base = cargo_environment(root, rustc, opts.jobs)
        samples = {n: [] for n in tools}
        names = list(tools)
        for i in range(opts.runs):
            for n in names[i % len(names) :] + names[: i % len(names)]:
                for dest in targets:
                    if dest.exists():
                        shutil.rmtree(dest)
                    dest.mkdir()
                with tempfile.TemporaryDirectory(dir=root, prefix="r-") as tmp:
                    private = Path(tmp)
                    worker = multiprocessing.get_context("spawn").Process(
                        target=trial,
                        args=(
                            n,
                            tools[n],
                            cargo,
                            base,
                            projects,
                            targets,
                            private,
                            opts.verify_bin,
                        ),
                    )
                    worker.start()
                    worker.join()
                    if worker.exitcode != 0:
                        raise RuntimeError(f"{n} trial failed")
                    row = json.loads((private / "sample.json").read_text())
                    samples[n].append(row)
                    print(i + 1, n, row, file=sys.stderr, flush=True)
        medians = {
            n: {
                k: statistics.median(r[k] for r in rows)
                for k in rows[0]
                if rows[0][k] is not None
            }
            for n, rows in samples.items()
        }
        print(
            json.dumps(
                dict(
                    versions=versions,
                    project=opts.project.name
                    if opts.project
                    else "locked regex application",
                    rustc=run([rustc, "-V"], base, projects[0]).decode().strip(),
                    kernel=platform.release(),
                    runs=opts.runs,
                    copies=opts.copies,
                    jobs_per_copy=opts.jobs,
                    samples=samples,
                    median=medians,
                ),
                indent=2,
            )
        )


if __name__ == "__main__":
    main()
