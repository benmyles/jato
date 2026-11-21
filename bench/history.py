#!/usr/bin/env python3
"""Measure packed-store growth across source revisions of one Rust library."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import resource
import shutil
import subprocess
import tempfile
import time


def run(args, project, env):
    return subprocess.check_output(args, cwd=project, env=env, stderr=subprocess.PIPE)


def source(revision):
    marker = "x" * (1 + 37 * revision)
    return f'''pub const TABLE: &[u8] = include_bytes!("table.bin");
#[no_mangle]
pub extern "C" fn revision_{marker}(index: usize) -> usize {{
    TABLE[index % TABLE.len()] as usize + {revision}
}}
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jato", default="jato")
    parser.add_argument("--previous-jato", required=True)
    parser.add_argument("--variants", type=int, default=8)
    parser.add_argument("--edit-asset", action="store_true", help="edit one byte of a 16 MiB static asset between builds")
    parser.add_argument("--scratch", default=os.environ.get("TMPDIR") or "/var/tmp")
    opts = parser.parse_args()
    if not 2 <= opts.variants <= 64:
        parser.error("--variants must be between 2 and 64")
    rustc = Path(subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip()) / "bin/rustc"
    cargo = rustc.with_name("cargo")
    with tempfile.TemporaryDirectory(dir=opts.scratch, prefix="jato-history-") as tmp:
        root = Path(tmp).resolve()
        project = root / "project"
        src = project / "src"
        src.mkdir(parents=True)
        (project / "Cargo.toml").write_text('[package]\nname="jato-history-bench"\nversion="0.1.0"\nedition="2021"\n')
        asset = src / "table.bin"
        size = (16 if opts.edit_asset else 4) * 1024 * 1024 + 19
        asset.write_bytes(hashlib.shake_256(b"jato history v1").digest(size))
        lib = src / "lib.rs"
        if opts.edit_asset:
            lib.write_text('pub static TABLE: &[u8] = include_bytes!("table.bin");')

        def edit(revision):
            if opts.edit_asset:
                with asset.open("r+b") as file:
                    file.seek(size // 2)
                    file.write(bytes([revision]))
            else:
                lib.write_text(source(revision))

        edit(0)
        target = root / "target"
        env = {k: v for k, v in os.environ.items() if not k.startswith(("CARGO_", "JATO_", "RUSTC_", "KACHE_", "RUSTFLAGS"))}
        env.update(CARGO_HOME=str(root / "cargo"), CARGO_NET_OFFLINE="true", CARGO_BUILD_JOBS="4",
                   CARGO_TARGET_DIR=str(target), CARGO_BUILD_BUILD_DIR=str(target), RUSTC=str(rustc),
                   RUSTC_WRAPPER="", RUSTC_WORKSPACE_WRAPPER="", JATO_STORAGE="packed", JATO_MAX_SIZE="1GiB",
                   PATH=os.pathsep.join([str(rustc.parent), "/usr/bin", "/bin", env["PATH"]]))
        run([cargo, "generate-lockfile", "--offline"], project, env)
        report = {}
        for label, tool in [("previous_jato", opts.previous_jato), ("jato", opts.jato)]:
            binary = str(Path(shutil.which(tool) or tool).resolve(strict=True))
            cfg = dict(env, RUSTC_WRAPPER=binary, JATO_CACHE_DIR=str(root / label))
            if target.exists():
                shutil.rmtree(target)
            command = [cargo, "build", "--release", "--lib", "--locked", "-q"]
            sample = []
            archive = target / "release/libjato_history_bench.rlib"
            for revision in range(opts.variants):
                edit(revision)
                writes = resource.getrusage(resource.RUSAGE_CHILDREN).ru_oublock
                start = time.perf_counter()
                run(command, project, cfg)
                seconds = time.perf_counter() - start
                writes = resource.getrusage(resource.RUSAGE_CHILDREN).ru_oublock - writes
                if revision == 0:
                    initial = hashlib.sha256(archive.read_bytes()).hexdigest()
                stats = json.loads(run([binary, "stats", "--json"], project, cfg))
                assert stats["entries"] == revision + 1, "a source revision was not stored"
                sample.append({k: stats[k] for k in ("entries", "artifact_bytes", "compressed_bytes", "charged_bytes", "database_bytes")})
                sample[-1]["seconds"] = round(seconds, 6)
                sample[-1]["filesystem_output_blocks"] = writes
                sample[-1]["packed_reused_bytes"] = stats["counters"].get("packed_reused_bytes", 0)
            shutil.rmtree(target)
            edit(0)
            start = time.perf_counter()
            run(command, project, cfg)
            restore = time.perf_counter() - start
            assert hashlib.sha256(archive.read_bytes()).hexdigest() == initial, "restored library changed"
            stats = json.loads(run([binary, "stats", "--json"], project, cfg))
            assert stats["counters"].get("hit", 0) > 0, "the first source revision did not restore"
            report[label] = dict(version=run([binary, "--version"], project, cfg).decode().strip(),
                                 revisions=sample, restore_seconds=round(restore, 6), archive_sha256=initial)
        print(json.dumps(dict(rustc=run([rustc, "-V"], project, env).decode().strip(),
                              variants=opts.variants, edit_asset=opts.edit_asset, samples=report), indent=2))


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        raise SystemExit(error.stderr.decode(errors="replace")) from error
