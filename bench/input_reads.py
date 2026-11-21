#!/usr/bin/env python3
"""Measure failed Cargo checks in a crate with a large, unchanged embedded input."""

import argparse
import json
import os
import platform
import re
import statistics
import subprocess
import tempfile
import time
from pathlib import Path

from compare import binary, run
from worktrees import cargo_environment, private_environment


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jato", default="jato")
    parser.add_argument("--previous-jato", required=True)
    parser.add_argument("--mib", type=int, default=64)
    parser.add_argument("--runs", type=int, default=9)
    parser.add_argument("--scratch", default=os.environ.get("TMPDIR") or "/var/tmp")
    opts = parser.parse_args()
    if min(opts.mib, opts.runs) < 1:
        parser.error("use a positive size and number of runs")
    rustc = (
        Path(
            subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip()
        )
        / "bin/rustc"
    )
    tools = {"previous_jato": binary(opts.previous_jato), "jato": binary(opts.jato)}
    with tempfile.TemporaryDirectory(dir=opts.scratch, prefix="ji-") as tmp:
        root = Path(tmp).resolve()
        project = root / "project"
        project.mkdir()
        (project / "Cargo.toml").write_text(
            '[package]\nname="embedded"\nversion="0.1.0"\nedition="2021"\n[lib]\npath="lib.rs"\n'
        )
        asset = project / "asset.bin"
        with asset.open("wb") as f:
            f.truncate(opts.mib << 20)
        source = project / "lib.rs"
        original = 'pub static DATA: &[u8] = include_bytes!("asset.bin");\n'
        source.write_text(original)
        base = cargo_environment(root, rustc, 1)
        cargo = rustc.with_name("cargo")
        run([cargo, "generate-lockfile", "--offline"], base, project)
        command = [cargo, "check", "--locked", "--release", "-q"]
        config = root / "empty.toml"
        config.touch()
        envs = {}
        results = {}
        for name, exe in tools.items():
            private = root / name
            private.mkdir()
            env = private_environment(base, private, config, str(exe))
            env.update(
                CARGO_TARGET_DIR=str(private / "target"),
                CARGO_BUILD_BUILD_DIR=str(private / "target"),
            )
            envs[name] = env
            run(command, env, project)
            stats = json.loads(run([exe, "stats", "--json"], env, project))
            assert stats["entries"] == 1, stats
            results[name] = {
                "version": run([exe, "--version"], env, project).decode().strip(),
                "seconds": [],
            }

        def failure(args, env):
            start = time.perf_counter()
            output = subprocess.run(
                args, env=env, cwd=project, capture_output=True, check=False
            )
            elapsed = time.perf_counter() - start
            assert (
                output.returncode == 101 and b"jato benchmark error" in output.stderr
            ), output.stderr
            return elapsed

        for i in range(opts.runs + 1):
            source.write_text(
                f'compile_error!("jato benchmark error {i}");\n' + original
            )
            names = list(tools)
            for n in names[i % 2 :] + names[: i % 2]:
                elapsed = failure(command, envs[n])
                if i:
                    results[n]["seconds"].append(round(elapsed, 6))
        for name, exe in tools.items():
            # -j1 leaves no spare jobserver tokens. All wrapper reads are in its
            # own trace file; rustc's reads are in a separate child process.
            prefix = root / (name + "-trace")
            failure(
                [
                    "strace",
                    "-ff",
                    "-qq",
                    "-yy",
                    "-s",
                    "128",
                    "-e",
                    "trace=read,pread64,execve",
                    "-o",
                    prefix,
                    *command,
                ],
                envs[name],
            )
            traces = []
            for path in root.glob(prefix.name + ".*"):
                lines = path.read_text(errors="replace").splitlines()
                if any(
                    line.startswith(f'execve("{exe}"')
                    and '"--crate-name", "embedded"' in line
                    for line in lines
                ):
                    traces.append(lines)
            assert len(traces) == 1, (
                f"expected one compiler wrapper trace, found {len(traces)}"
            )
            pattern = re.compile(
                r"^(?:read|pread64)\(\d+<" + re.escape(str(asset)) + r">.* = (\d+)$"
            )
            count = sum(int(m[1]) for line in traces[0] if (m := pattern.match(line)))
            results[name]["wrapper_asset_bytes_read"] = count
            results[name]["median_seconds"] = statistics.median(
                results[name]["seconds"]
            )
        print(
            json.dumps(
                {
                    "rustc": run([rustc, "-V"], base, project).decode().strip(),
                    "kernel": platform.release(),
                    "jobs": 1,
                    "asset_bytes": asset.stat().st_size,
                    "runs": opts.runs,
                    "samples": results,
                },
                indent=2,
            )
        )


if __name__ == "__main__":
    main()
