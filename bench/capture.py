#!/usr/bin/env python3
"""Trace packed staging I/O and time repeated byte-identical native captures on Linux."""

import argparse
import hashlib
import json
import os
import platform
import re
import shutil
import statistics
import subprocess
import tempfile
import time
from pathlib import Path

from compare import binary, run
from worktrees import cargo_environment, private_environment


def wrapper_trace(exe, command, env, project, prefix):
    run(
        [
            "strace",
            "-ff",
            "-qq",
            "-yy",
            "-s",
            "96",
            "-e",
            "trace=execve,write,writev,pwrite64",
            "-o",
            prefix,
            *command,
        ],
        env,
        project,
    )
    traces = []
    for f in prefix.parent.glob(prefix.name + ".*"):
        lines = f.read_text(errors="replace").splitlines()
        if any(
            line.startswith(f'execve("{exe}"') and '"--crate-name", "capture"' in line
            for line in lines
        ):
            traces.append(lines)
    assert len(traces) == 1, f"expected one compiler wrapper trace, found {len(traces)}"
    return traces[0]


def packed_writes(exe, cargo, env, project, root, runs):
    command = [cargo, "build", "--release", "--lib", "--locked", "-q"]
    env = dict(env, JATO_STORAGE="packed")
    cold = wrapper_trace(exe, command, env, project, root / "cold")
    store = re.escape(env["JATO_CACHE_DIR"])
    anonymous = re.compile(
        r"^(?:write|writev|pwrite64)\(\d+<"
        + store
        + r"/[^>]*(?:>\(deleted\)|\(deleted\)>).* = (\d+)$"
    )
    db = re.compile(
        r"^pwrite64\(\d+<" + store + r"/artifacts-v1\.sqlite3(?:-wal)?>.* = (\d+)$"
    )
    frame = re.compile(
        r"^(?:write|pwrite64)\(\d+<" + store + r"/frames-v1/\.pending>.* = (\d+)$"
    )

    def total(pattern, lines):
        return sum(int(m[1]) for line in lines if (m := pattern.match(line)))

    stats = json.loads(run([exe, "stats", "--json"], env, project))
    assert stats["entries"] == 1
    written = total(db, cold)
    payload = total(frame, cold)
    assert payload == stats.get("frame_file_bytes", 0), (
        "frame writes missing from trace"
    )
    assert written + payload >= stats["compressed_bytes"], (
        "payload writes missing from trace"
    )
    target = Path(env["CARGO_TARGET_DIR"])
    archive = target / "release/libcapture.rlib"
    with archive.open("rb") as stream:
        expected = hashlib.file_digest(stream, "sha256")
    shutil.rmtree(target)
    warm = wrapper_trace(exe, command, env, project, root / "warm")
    with archive.open("rb") as stream:
        assert hashlib.file_digest(stream, "sha256").digest() == expected.digest()
    restored = json.loads(run([exe, "stats", "--json"], env, project))
    assert restored["counters"].get("hit", 0) == 1
    times = []
    for _ in range(runs):
        shutil.rmtree(target)
        start = time.perf_counter()
        run(command, env, project)
        times.append(round(time.perf_counter() - start, 6))
        with archive.open("rb") as stream:
            assert hashlib.file_digest(stream, "sha256").digest() == expected.digest()
    target_writes = re.compile(
        r"^(?:write|writev|pwrite64)\(\d+<"
        + re.escape(str(target))
        + r"/[^>]+>.* = (\d+)$"
    )
    return {
        "untraced_restore_seconds": times,
        "median_restore_seconds": statistics.median(times),
        "logical_restored_bytes": stats["artifact_bytes"],
        "restore_output_bytes_written": total(target_writes, warm),
        "restore_archive_sha256": expected.hexdigest(),
        "anonymous_staging_bytes_written": total(anonymous, cold),
        "sqlite_bytes_written": written,
        "frame_file_bytes_written": payload,
        "compressed_payload_bytes": stats["compressed_bytes"],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jato", default="jato")
    parser.add_argument("--previous-jato", required=True)
    parser.add_argument("--mib", type=int, default=32)
    parser.add_argument(
        "--sparse",
        action="store_true",
        help="embed long zero regions separated by distinct marker bytes",
    )
    parser.add_argument("--runs", type=int, default=7)
    parser.add_argument("--scratch", default=os.environ.get("TMPDIR") or "/var/tmp")
    opts = parser.parse_args()
    if min(opts.mib, opts.runs) < 1:
        parser.error("use a positive size and number of edit trials")
    rustc = (
        Path(
            subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip()
        )
        / "bin/rustc"
    )
    cargo = rustc.with_name("cargo")
    tools = {"jato": binary(opts.jato), "previous_jato": binary(opts.previous_jato)}
    with tempfile.TemporaryDirectory(dir=opts.scratch, prefix="jc-io-") as tmp:
        root = Path(tmp).resolve()
        project = root / "project"
        project.mkdir()
        src = project / "src"
        src.mkdir()
        (project / "Cargo.toml").write_text(
            '[package]\nname="capture"\nversion="0.1.0"\nedition="2021"\n'
        )
        asset = src / "asset.bin"
        if opts.sparse:
            with asset.open("wb") as stream:
                stream.truncate(opts.mib << 20)
                for i in range(opts.mib):
                    stream.seek((i << 20) + 19)
                    stream.write(bytes([1 + i % 254]))
        else:
            asset.write_bytes(
                hashlib.shake_256(b"jato publication fixture").digest(opts.mib << 20)
            )
        (src / "lib.rs").write_text(
            'pub static ASSET: &[u8] = include_bytes!("asset.bin");'
        )
        main_rs = src / "main.rs"
        source = """use std::{fs::File,io::Read};
fn main(){
    let mut file = File::open(std::env::args_os().nth(1).unwrap()).unwrap();
    let mut buf = [0u8;65536];
    for part in capture::ASSET.chunks(buf.len()) {
        file.read_exact(&mut buf[..part.len()]).unwrap();
        assert!(part == &buf[..part.len()]);
    }
    assert_eq!(file.read(&mut buf).unwrap(),0);
    println!("{}",capture::ASSET.len());
}
"""

        def edit(i):
            main_rs.write_text(source + f"// editor revision {i:04}\n")

        edit(0)
        base = cargo_environment(root, rustc, 4)
        run([cargo, "generate-lockfile", "--offline"], base, project)
        config = root / "empty.toml"
        config.touch()
        results = {}
        envs = {}

        def environment(work, exe):
            work.mkdir()
            return dict(
                private_environment(base, work, config, str(exe)),
                CARGO_TARGET_DIR=str(work / "target"),
                CARGO_BUILD_BUILD_DIR=str(work / "target"),
            )

        for name, exe in tools.items():
            private = root / name
            private.mkdir()
            trace_env = environment(private / "packed", exe)
            envs[name] = dict(environment(private / "auto", exe), JATO_LOG="timings")
            results[name] = {
                "version": run([exe, "--version"], base, project).decode().strip(),
                "cold_packed": packed_writes(exe, cargo, trace_env, project, private, opts.runs),
                "builds": [],
            }
        for revision in range(opts.runs + 1):
            edit(revision)
            for name in list(tools) if revision % 2 == 0 else reversed(tools):
                cfg = envs[name]
                row = results[name]
                start = time.perf_counter()
                out = subprocess.run(
                    [cargo, "build", "--release", "--locked", "-q"],
                    env=cfg,
                    cwd=project,
                    capture_output=True,
                    check=True,
                )
                seconds = time.perf_counter() - start
                matches = re.findall(
                    rb"jato \[capture\] compile (\d+) us, validate (\d+) us, store (\d+) us",
                    out.stderr,
                )
                elapsed = {"seconds": round(seconds, 6)}
                if matches:
                    elapsed["native_store_us"] = int(matches[-1][2])
                row["builds"].append(elapsed)
                program = Path(cfg["CARGO_TARGET_DIR"]) / "release/capture"
                assert (
                    run([program, asset], cfg, project)
                    == f"{asset.stat().st_size}\n".encode()
                )
                with program.open("rb") as stream:
                    sha = hashlib.file_digest(stream, "sha256").hexdigest()
                if revision == 0:
                    row["executable_sha256"] = sha
                    row["executable_bytes"] = program.stat().st_size
                assert sha == row["executable_sha256"], (
                    "comment edit changed executable bytes"
                )
        edit(0)
        for name, exe in tools.items():
            cfg = envs[name]
            row = results[name]
            stats = json.loads(run([exe, "stats", "--json"], cfg, project))
            assert stats["entries"] >= opts.runs + 2
            row["sealed_reused_bytes"] = stats["counters"].get("sealed_reused_bytes", 0)
            row["sealed_objects"] = stats["sealed_objects"]
            row["median_edit_seconds"] = statistics.median(
                v["seconds"] for v in row["builds"][1:]
            )
            shutil.rmtree(cfg["CARGO_TARGET_DIR"])
            run([cargo, "build", "--release", "--locked", "-q"], cfg, project)
            program = Path(cfg["CARGO_TARGET_DIR"]) / "release/capture"
            assert (
                run([program, asset], cfg, project)
                == f"{asset.stat().st_size}\n".encode()
            )
            with program.open("rb") as stream:
                assert (
                    hashlib.file_digest(stream, "sha256").hexdigest()
                    == row["executable_sha256"]
                )
            after = json.loads(run([exe, "stats", "--json"], cfg, project))
            row["restore_hits"] = after["counters"]["hit"] - stats["counters"].get(
                "hit", 0
            )
            assert row["restore_hits"] == 2
        print(
            json.dumps(
                {
                    "rustc": run([rustc, "-V"], base, project).decode().strip(),
                    "kernel": platform.release(),
                    "runs": opts.runs,
                    "asset_bytes": asset.stat().st_size,
                    "sparse": opts.sparse,
                    "samples": results,
                },
                indent=2,
            )
        )


if __name__ == "__main__":
    main()
