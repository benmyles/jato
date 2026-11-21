#!/usr/bin/env python3
"""Measure local cache growth and verified restores across revisions of a Rust library."""

import argparse
import contextlib
import hashlib
import json
import os
import re
import resource
import shutil
import subprocess
import tempfile
import time
from pathlib import Path

from capture import wrapper_trace
from compare import binary, extent_usage, run, tree_bytes
from worktrees import cargo_environment, counters, private_environment


def archive_hash(path):
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def source(revision):
    marker = "x" * (1 + 37 * revision)
    return f"""pub const TABLE: &[u8] = include_bytes!("table.bin");
#[no_mangle]
pub extern "C" fn revision_{marker}(index: usize) -> usize {{
    TABLE[index % TABLE.len()] as usize + {revision}
}}
"""


def verify_library(project, archive, env, revision, symbol):
    # Link and execute a downstream crate outside the timed build. Its own
    # workspace stays outside the compiler's tracked source directories.
    directory = project.parent / "consumer"
    directory.mkdir(exist_ok=True)
    probe = directory / "probe.rs"
    call = ""
    if symbol:
        name = "revision_" + "x" * (1 + 37 * revision)
        call = (
            f"assert_eq!(jato_history_bench::{name}(0), data[0] as usize + {revision});"
        )
    probe.write_text(
        """use std::{fs::File, io::Read};
fn main() {
    let data = jato_history_bench::TABLE;
    let mut asset = File::open(std::env::args_os().nth(1).unwrap()).unwrap();
    let mut buf = [0u8; 65536];
    for part in data.chunks(buf.len()) {
        asset.read_exact(&mut buf[..part.len()]).unwrap();
        assert!(part == &buf[..part.len()], "incorrect embedded content");
    }
    assert_eq!(asset.read(&mut buf).unwrap(), 0);
"""
        + call
        + "\n}\n"
    )
    program = directory / "probe"
    run(
        [
            env["RUSTC"],
            "--edition=2021",
            "-Clinker=/usr/bin/cc",
            "--extern",
            f"jato_history_bench={archive}",
            probe,
            "-o",
            program,
        ],
        env,
        directory,
    )
    run([program, project / "src/table.bin"], env, directory)


def revisions(label, exe, project, target, env, edit, count, symbol, trace_edited):
    command = [env["CARGO"], "build", "--release", "--lib", "--locked", "-q"]
    archive = target / "release/libjato_history_bench.rlib"
    jato = label in {"jato", "previous_jato"}
    rows = []
    for revision in range(count):
        edit(revision)
        before = resource.getrusage(resource.RUSAGE_CHILDREN).ru_oublock
        start = time.perf_counter()
        run(command, env, project)
        row = {"seconds": round(time.perf_counter() - start, 6)}
        if jato:
            # Jato compiles as a child. Peer daemon filesystem counters
            # would omit their compiler processes.
            row["filesystem_output_blocks"] = (
                resource.getrusage(resource.RUSAGE_CHILDREN).ru_oublock - before
            )
            stats = json.loads(run([exe, "stats", "--json"], env, project))
            assert stats["entries"] == revision + 1, "a source revision was not stored"
            for key in (
                "entries",
                "artifact_bytes",
                "compressed_bytes",
                "charged_bytes",
                "database_bytes",
            ):
                row[key] = stats[key]
            row["packed_reused_bytes"] = stats["counters"].get("packed_reused_bytes", 0)
            row["sealed_bytes"] = stats["sealed_bytes"]
            row["frame_file_bytes"] = stats.get("frame_file_bytes", 0)
            row["patched_files"] = stats.get("patched_files", 0)
            row["patch_bytes"] = stats.get("patch_bytes", 0)
        if revision in {0, count - 1}:
            checksum = archive_hash(archive)
            if revision == 0:
                initial = checksum
            else:
                last = checksum
        verify_library(project, archive, env, revision, symbol)
        rows.append(row)
    shutil.rmtree(target)
    edit(0)
    before_hits = counters(label, exe, env, project)["hits"]
    start = time.perf_counter()
    run(command, env, project)
    elapsed = time.perf_counter() - start
    assert archive_hash(archive) == initial, "restored library changed"
    hits = counters(label, exe, env, project)["hits"] - before_hits
    verify_library(project, archive, env, 0, symbol)
    if jato:
        assert hits == 1, "the first source revision did not restore"
    # Restore the edited artifact too. In automatic storage the first and
    # last revisions can use different on-disk representations.
    shutil.rmtree(target)
    edit(count - 1)
    before = counters(label, exe, env, project)["hits"]
    start = time.perf_counter()
    run(command, env, project)
    latest = time.perf_counter() - start
    assert archive_hash(archive) == last, "edited archive changed on restore"
    latest_hits = counters(label, exe, env, project)["hits"] - before
    if jato:
        assert latest_hits == 1
    verify_library(project, archive, env, count - 1, symbol)
    trace = {}
    if trace_edited and jato:
        shutil.rmtree(target)
        prefix = project.parent / (label + "-restore")
        lines = wrapper_trace(
            exe, command, env, project, prefix, "jato_history_bench", "read,pread64"
        )
        pattern = re.compile(
            r"^(?:read|pread64)\(\d+<"
            + re.escape(str(target))
            + r"/[^>]*(?:/#[0-9]+|/part-[^>]+)>.* = (\d+)$"
        )
        trace["edited_staging_bytes_read"] = sum(
            int(m[1]) for line in lines if (m := pattern.match(line))
        )
        assert archive_hash(archive) == last
        assert counters(label, exe, env, project)["hits"] == before + latest_hits + 1
    return {
        "version": run([exe, "--version"], env, project).decode().strip(),
        "revisions": rows,
        "restore_seconds": round(elapsed, 6),
        "restore_hits": hits,
        "edited_restore_seconds": round(latest, 6),
        "edited_restore_hits": latest_hits,
        "edited_archive_sha256": last,
        **trace,
        "verified_revisions": count,
        "archive_sha256": initial,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jato", default="jato")
    parser.add_argument("--previous-jato", required=True)
    parser.add_argument(
        "--kache", help="include this kache binary with a private local server"
    )
    parser.add_argument(
        "--sccache", help="include this sccache binary with a private local server"
    )
    parser.add_argument("--variants", type=int, default=8)
    parser.add_argument(
        "--trace-edited",
        action="store_true",
        help="trace Jato reads of staged targets on a separate edited restore",
    )
    parser.add_argument("--storage", choices=["packed", "auto"], default="packed")
    workload = parser.add_mutually_exclusive_group()
    workload.add_argument(
        "--insert-asset",
        action="store_true",
        help="insert bytes near the beginning of a 16 MiB asset between builds",
    )
    workload.add_argument(
        "--edit-asset",
        action="store_true",
        help="edit one byte of a 16 MiB static asset between builds",
    )
    parser.add_argument("--scratch", default=os.environ.get("TMPDIR") or "/var/tmp")
    opts = parser.parse_args()
    if not 2 <= opts.variants <= 64:
        parser.error("--variants must be between 2 and 64")
    rustc = (
        Path(
            subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip()
        )
        / "bin/rustc"
    )
    cargo = rustc.with_name("cargo")
    with tempfile.TemporaryDirectory(dir=opts.scratch, prefix="jato-history-") as tmp:
        root = Path(tmp).resolve()
        project = root / "project"
        src = project / "src"
        src.mkdir(parents=True)
        (project / "Cargo.toml").write_text(
            '[package]\nname="jato-history-bench"\nversion="0.1.0"\nedition="2021"\n'
        )
        asset = src / "table.bin"
        size = (16 if opts.edit_asset or opts.insert_asset else 4) * 1024 * 1024 + 19
        asset.write_bytes(hashlib.shake_256(b"jato history v1").digest(size))
        lib = src / "lib.rs"
        original = asset.read_bytes() if opts.insert_asset else None
        if opts.edit_asset or opts.insert_asset:
            lib.write_text('pub static TABLE: &[u8] = include_bytes!("table.bin");')

        def edit(revision):
            if opts.insert_asset:
                offset = 8192
                with asset.open("wb") as file:
                    file.write(memoryview(original)[:offset])
                    file.write(b"x" * revision)
                    file.write(memoryview(original)[offset:])
            elif opts.edit_asset:
                with asset.open("r+b") as file:
                    file.seek(size // 2)
                    file.write(bytes([revision]))
            else:
                lib.write_text(source(revision))

        edit(0)
        target = root / "target"
        env = cargo_environment(root, rustc, 4)
        env.update(
            CARGO_TARGET_DIR=str(target),
            CARGO_BUILD_BUILD_DIR=str(target),
            JATO_STORAGE=opts.storage,
        )
        run([cargo, "generate-lockfile", "--offline"], env, project)
        config = root / "empty.toml"
        config.touch()
        report = {}
        tools = {
            "previous_jato": opts.previous_jato,
            "jato": opts.jato,
            "kache": opts.kache,
            "sccache": opts.sccache,
        }
        for label, name in tools.items():
            if not name:
                continue
            exe = binary(name)
            private = root / label
            private.mkdir()
            if target.exists():
                shutil.rmtree(target)
            target.mkdir()
            cfg = private_environment(env, private, config, str(exe))
            cfg["SCCACHE_BASEDIRS"] = os.pathsep.join(map(str, [project, target]))
            with contextlib.ExitStack() as servers:
                server = {
                    "kache": (["daemon", "start"], ["daemon", "stop"]),
                    "sccache": (["--start-server"], ["--stop-server"]),
                }.get(label)
                if server:
                    run([exe, *server[0]], cfg, project)
                    servers.callback(run, [exe, *server[1]], cfg, project)
                row = revisions(
                    label,
                    exe,
                    project,
                    target,
                    cfg,
                    edit,
                    opts.variants,
                    not (opts.edit_asset or opts.insert_asset),
                    opts.trace_edited,
                )
            store = private / "store"
            row.update(store_bytes=tree_bytes(store), **extent_usage(store))
            report[label] = row
        print(
            json.dumps(
                {
                    "rustc": run([rustc, "-V"], env, project).decode().strip(),
                    "variants": opts.variants,
                    "edit_asset": opts.edit_asset,
                    "insert_asset": opts.insert_asset,
                    "storage": opts.storage,
                    "samples": report,
                },
                indent=2,
            )
        )


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        raise SystemExit(error.stderr.decode(errors="replace")) from error
