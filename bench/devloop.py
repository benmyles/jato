#!/usr/bin/env python3
"""Build actual edits and reversions in a live Cargo incremental session."""

import argparse
import contextlib
import json
import os
import platform
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from compare import binary, run
from worktrees import cargo_environment, counters, private_environment


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ["jato", "kache", "sccache"]:
        parser.add_argument("--" + name, default=name)
    parser.add_argument("--previous-jato")
    parser.add_argument(
        "--tools",
        nargs="+",
        choices=["cargo", "jato", "kache", "sccache"],
        default=["cargo", "jato", "kache", "sccache"],
    )
    parser.add_argument("--project", type=Path, help="locked workspace to copy")
    parser.add_argument(
        "--source", help="source file containing fn main(), relative to --project"
    )
    parser.add_argument(
        "--verify-bin", help="name of the executable to run after every build"
    )
    parser.add_argument("--runs", type=int, default=7)
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--scratch", default=os.environ.get("TMPDIR") or "/var/tmp")
    opts = parser.parse_args()
    if min(opts.runs, opts.jobs) < 1 or (
        opts.project and not (opts.source and opts.verify_bin)
    ):
        parser.error(
            "use positive jobs and runs, and --source plus --verify-bin with --project"
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
        n: subprocess.check_output([exe, "--version"], text=True).strip()
        for n, exe in tools.items()
    }
    with (
        tempfile.TemporaryDirectory(dir=opts.scratch, prefix="jl-") as scratch,
        contextlib.ExitStack() as servers,
    ):
        root = Path(scratch).resolve()
        project = root / "project"
        original = (opts.project or Path(__file__).parent / "regex-app").resolve()

        def ignore(directory, names):
            excluded = {".git", "__pycache__"}
            if Path(directory) == original:
                excluded.add("target")
            return excluded.intersection(names)

        shutil.copytree(original, project, symlinks=True, ignore=ignore)
        if not opts.project:
            (project / "Cargo.toml.in").rename(project / "Cargo.toml")
        source = (project / (opts.source or "src/main.rs")).resolve(strict=True)
        if not source.is_relative_to(project):
            parser.error("--source must resolve inside the project copy")
        template = source.read_text()
        match = re.search(r"\bfn\s+main\s*\(\s*\)\s*(?:->[^{}]+)?\{", template)
        if match is None:
            parser.error("could not locate the executable entry point")

        def edit(revision):
            branch = (
                '\n    if std::env::var_os("JATO_BENCH_PROBE").is_some() { '
                f'println!("revision-{revision}"); '
                "std::process::exit(0); }\n"
            )
            temporary = source.with_suffix(".bench-save")
            temporary.write_text(
                template[: match.end()] + branch + template[match.end() :]
            )
            temporary.replace(source)

        base = cargo_environment(root, rustc, opts.jobs)
        command = [cargo, "build", "--locked", "-q"]
        config = root / "empty.toml"
        config.touch()
        envs = {}
        samples = {n: {} for n in tools}
        for n, exe in tools.items():
            private = root / n
            private.mkdir()
            target = private / "target"
            target.mkdir()
            env = private_environment(
                base, private, config, "" if n == "cargo" else str(exe)
            )
            env.update(
                CARGO_TARGET_DIR=str(target),
                CARGO_BUILD_BUILD_DIR=str(target),
                CARGO_PROFILE_DEV_INCREMENTAL="true",
                SCCACHE_BASEDIRS=os.pathsep.join(map(str, [project, target])),
            )
            envs[n] = env
            server = {
                "kache": (["daemon", "start"], ["daemon", "stop"]),
                "sccache": (["--start-server"], ["--stop-server"]),
            }.get(n)
            if server:
                run([exe, *server[0]], env, project)
                servers.callback(run, [exe, *server[1]], env, project)

        def measure(n, phase, revision):
            env = envs[n]
            exe = tools[n]
            before = counters(n, exe, env, project)
            start = time.perf_counter()
            run(command, env, project)
            elapsed = time.perf_counter() - start
            output = (
                Path(env["CARGO_TARGET_DIR"])
                / "debug"
                / (opts.verify_bin or "jato-regex-bench")
            )
            assert (
                run([output], dict(env, JATO_BENCH_PROBE="1"), project)
                == f"revision-{revision}\n".encode()
            ), "stale executable"
            after = counters(n, exe, env, project)
            row = dict(
                seconds=round(elapsed, 6),
                **{k: v - before[k] for k, v in after.items()},
            )
            samples[n].setdefault(phase, []).append(row)
            print(n, phase, row, file=sys.stderr, flush=True)

        edit(0)
        for n in tools:
            measure(n, "cold", 0)
            measure(n, "fresh", 0)
        names = list(tools)
        for i in range(opts.runs):
            for n in names[i % len(names) :] + names[: i % len(names)]:
                edit(i + 1)
                measure(n, "edit", i + 1)
                edit(0)
                measure(n, "undo", 0)
        servers.close()
        medians = {
            n: {
                p: statistics.median(t["seconds"] for t in rows)
                for p, rows in phases.items()
            }
            for n, phases in samples.items()
        }
        print(
            json.dumps(
                {
                    "versions": versions,
                    "project": opts.project.name
                    if opts.project
                    else "locked regex application",
                    "rustc": run([rustc, "-V"], base, project).decode().strip(),
                    "kernel": platform.release(),
                    "runs": opts.runs,
                    "jobs": opts.jobs,
                    "samples": samples,
                    "median_seconds": medians,
                },
                indent=2,
            )
        )


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        raise SystemExit((error.stderr or b"").decode(errors="replace")) from error
