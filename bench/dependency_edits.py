#!/usr/bin/env python3
"""Measure an unchanged fan-out of Rust consumers after editing a large producer."""

import argparse
import contextlib
import hashlib
import json
import os
import platform
import re
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from compare import binary, run
from worktrees import cargo_environment, counters, private_environment


def metadata(project, rustc, env, messages, clients, revision):
    files = {}
    for line in messages.splitlines():
        artifact = json.loads(line)
        if artifact["reason"] == "compiler-artifact":
            files[artifact["target"]["name"]] = next(
                Path(f) for f in artifact["filenames"] if f.endswith(".rmeta")
            )
    # Evaluate a type from every downstream crate with the real compiler.
    p = Path(env["CARGO_TARGET_DIR"])
    check = p / "verify.rs"
    check.write_text(
        "\n".join(f"const _: [(); {revision}] = {c}::SEEN;" for c in clients)
    )
    args = [
        rustc,
        "--edition=2021",
        "--crate-type=lib",
        "--emit=metadata",
        "-L",
        f"dependency={files['producer'].parent}",
    ]
    for c in clients:
        args += ["--extern", f"{c}={files[c]}"]
    run([*args, check, "-o", p / "verified.rmeta"], env, project)
    return files["producer"].stat().st_size


def check(command, env, project, fail):
    start = time.perf_counter()
    p = subprocess.run(command, env=env, cwd=project, capture_output=True, check=False)
    seconds = time.perf_counter() - start
    if fail:
        assert p.returncode == 101 and b"unsupported producer revision" in p.stdout, (
            p.stderr
        )
    elif p.returncode != 0:
        raise RuntimeError(
            p.stderr.decode(errors="replace")
            + p.stdout.decode(errors="replace")[-2000:]
        )
    return seconds, p.stdout


def trace(exe, cmd, env, project, dest, fail):
    # A serial Cargo job keeps each wrapper's hashing in its own PID.
    _, messages = check(
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
            dest,
            *cmd,
            "-j1",
            "-p",
            "client0",
        ],
        env,
        project,
        fail,
    )
    reads = re.compile(
        r"^(?:read|pread64)\(\d+<[^>]*libproducer[^>]*\.rmeta>.* = (\d+)$"
    )
    for f in dest.parent.glob(dest.name + ".*"):
        with f.open(errors="replace") as stream:
            if any(
                l.startswith(f'execve("{exe}"') and '"--crate-name", "client0"' in l
                for l in stream
            ):
                return sum(
                    int(m[1]) for line in stream if (m := reads.match(line))
                ), messages
    raise RuntimeError("could not find consumer wrapper trace")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ["jato", "sccache", "kache"]:
        parser.add_argument("--" + name, default=name)
    parser.add_argument("--previous-jato", required=True)
    parser.add_argument(
        "--tools",
        nargs="+",
        choices=["cargo", "jato", "kache", "sccache"],
        default=["cargo", "jato", "kache", "sccache"],
    )
    parser.add_argument("--mib", type=int, default=8)
    parser.add_argument("--clients", type=int, default=4)
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--scratch", default=os.environ.get("TMPDIR") or "/var/tmp")
    opts = parser.parse_args()
    if (
        not 1 <= opts.runs <= 50
        or not 1 <= opts.clients <= 32
        or min(opts.mib, opts.jobs) < 1
    ):
        parser.error("use 1–50 trials, 1–32 consumers and positive jobs and asset size")
    rustc = (
        Path(
            subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip()
        )
        / "bin/rustc"
    )
    cargo = rustc.with_name("cargo")
    tools = {n: cargo if n == "cargo" else binary(getattr(opts, n)) for n in opts.tools}
    tools["previous_jato"] = binary(opts.previous_jato)
    with (
        tempfile.TemporaryDirectory(dir=opts.scratch, prefix="jd-") as tmp,
        contextlib.ExitStack() as servers,
    ):
        root = Path(tmp).resolve()
        project = root / "project"
        project.mkdir()
        clients = [f"client{i}" for i in range(opts.clients)]
        (project / "Cargo.toml").write_text(
            '[workspace]\nresolver="2"\nmembers='
            + json.dumps(["producer", *clients])
            + "\n"
        )
        for name in ["producer", *clients]:
            source = project / name / "src"
            source.mkdir(parents=True)
            manifest = f'[package]\nname="{name}"\nversion="0.1.0"\nedition="2021"\n'
            if name != "producer":
                manifest += '[dependencies]\nproducer={path="../producer"}\n'
                (source / "lib.rs").write_text(
                    'const _: ()=assert!(producer::REV < 200, "unsupported producer revision"); pub const SEEN: [();producer::REV]=[();producer::REV]; pub fn byte(i:usize)->u8 {producer::DATA[i]}\n'
                )
            (source.parent / "Cargo.toml").write_text(manifest)
        producer = project / "producer/src"
        (producer / "data.bin").write_bytes(
            hashlib.shake_256(b"dependency edits").digest(opts.mib << 20)
        )

        def edit(i):
            (producer / "lib.rs").write_text(
                f'pub const DATA: &[u8] = include_bytes!("data.bin"); pub const REV: usize = {i};\n'
            )

        edit(1)
        base = cargo_environment(root, rustc, opts.jobs)
        run([cargo, "generate-lockfile", "--offline"], base, project)
        command = [
            cargo,
            "check",
            "--release",
            "--locked",
            "-q",
            "--message-format=json",
        ]
        config = root / "empty.toml"
        config.touch()
        envs = {}
        report = {}
        for n, exe in tools.items():
            private = root / n
            private.mkdir()
            env = private_environment(
                base, private, config, "" if n == "cargo" else str(exe)
            )
            env.update(
                CARGO_TARGET_DIR=str(private / "target"),
                CARGO_BUILD_BUILD_DIR=str(private / "target"),
            )
            envs[n] = env
            server = {
                "kache": (["daemon", "start"], ["daemon", "stop"]),
                "sccache": (["--start-server"], ["--stop-server"]),
            }.get(n)
            if server:
                run([exe, *server[0]], env, project)
                servers.callback(run, [exe, *server[1]], env, project)
            report[n] = {
                "version": run([exe, "--version"], env, project).decode().strip(),
                "samples": {},
            }
        names = list(tools)
        for i in range(opts.runs + 1):
            for n in names[i % len(names) :] + names[: i % len(names)]:
                for phase, rev in (
                    [("cold", 1)]
                    if i == 0
                    else [("edit", i + 1), ("error", 200 + i), ("undo", 1)]
                ):
                    edit(rev)
                    env = envs[n]
                    before = counters(n, tools[n], env, project)
                    elapsed, msg = check(command, env, project, phase == "error")
                    if phase != "error":
                        report[n]["producer_metadata_bytes"] = metadata(
                            project, rustc, env, msg, clients, rev
                        )
                    after = counters(n, tools[n], env, project)
                    report[n]["samples"].setdefault(phase, []).append(
                        {
                            "seconds": round(elapsed, 6),
                            "hits": after.get("hits", 0) - before.get("hits", 0),
                        }
                    )
                print(i, n, flush=True, file=sys.stderr)
        for n in ["jato", "previous_jato"]:
            if n not in tools:
                continue
            for phase, rev in [("successful", 100), ("failed", 250)]:
                edit(rev)
                amount, msg = trace(
                    tools[n],
                    command,
                    envs[n],
                    project,
                    root / n / phase,
                    phase == "failed",
                )
                if phase == "successful":
                    metadata(project, rustc, envs[n], msg, clients[:1], rev)
                report[n][phase + "_consumer_rmeta_bytes_read"] = amount
            # Cargo may already have a usable external crate whose Jato producer
            # record was evicted. An older rmeta cannot be a current pipeline
            # publication. Both wrappers must hand the consumer back to rustc.
            env = envs[n]
            edit(101)
            producer_messages = run([*command, "-p", "producer"], env, project)
            files = [
                Path(path)
                for line in producer_messages.splitlines()
                if (m := json.loads(line)).get("reason") == "compiler-artifact"
                for path in m["filenames"] if path.endswith(".rmeta")
            ]
            assert len(files) == 1
            run([tools[n], "gc", "0"], env, project)
            old = time.time() - 300
            os.utime(files[0], (old, old))
            amount, messages = trace(tools[n], command, env, project, root / n / "uncertified", False)
            assert any(m.get("reason") == "compiler-artifact" and m["target"]["name"] == "producer" and m["fresh"]
                       for m in map(json.loads, messages.splitlines()))
            metadata(project, rustc, env, messages, clients[:1], 101)
            report[n]["uncertified_consumer_rmeta_bytes_read"] = amount
        for row in report.values():
            row["median_seconds"] = {
                k: statistics.median(t["seconds"] for t in v)
                for k, v in row["samples"].items()
            }
        print(
            json.dumps(
                {
                    "rustc": run([rustc, "-V"], base, project).decode().strip(),
                    "kernel": platform.release(),
                    "asset_mib": opts.mib,
                    "consumers": opts.clients,
                    "runs": opts.runs,
                    "jobs": opts.jobs,
                    "tools": report,
                },
                indent=2,
            )
        )


if __name__ == "__main__":
    main()
