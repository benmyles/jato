#!/usr/bin/env python3
"""Compare local Rust compiler wrappers using identical Cargo arguments and target paths."""
import argparse
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import sys
import tempfile
import time


def binary(name):
    return Path(shutil.which(name) or name).resolve(strict=True)


def run(args, env, project):
    return subprocess.run(args, cwd=project, env=env, check=True, capture_output=True).stdout


def tree_bytes(root):
    # Count ordinary files once per inode, including server data. Filesystem
    # compression and shared extents can change physical device usage.
    seen = set()
    total = 0
    for directory, _, files in os.walk(root):
        for name in files:
            try:
                info = (Path(directory) / name).stat()
            except FileNotFoundError:
                continue
            identity = info.st_dev, info.st_ino
            if identity not in seen:
                total += info.st_size
                seen.add(identity)
    return total


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project", type=Path, help="an existing locked Cargo project")
    parser.add_argument("--verify-bin", help="compare this release program's --version output")
    parser.add_argument("--jato", default="jato")
    parser.add_argument("--previous-jato")
    parser.add_argument("--kache", default="kache")
    parser.add_argument("--sccache", default="sccache")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--scratch", default=os.environ.get("TMPDIR") or "/var/tmp")
    options = parser.parse_args()
    if min(options.jobs, options.runs) < 1:
        parser.error("--jobs and --runs must be positive")
    tools = {"cargo": None, "jato": binary(options.jato), "kache": binary(options.kache),
             "sccache": binary(options.sccache)}
    if options.previous_jato:
        tools["previous_jato"] = binary(options.previous_jato)
    rustc = binary(str(Path(subprocess.check_output(["rustc", "--print", "sysroot"],
                                                    text=True).strip()) / "bin/rustc"))
    cargo = rustc.with_name("cargo")
    versions = {k: subprocess.check_output([v or cargo, "--version"], text=True).strip()
                for k, v in tools.items()}
    with tempfile.TemporaryDirectory(dir=options.scratch, prefix="jc-") as scratch:
        root = Path(scratch).resolve()
        target = root / "target"
        if options.project:
            project = options.project.resolve(strict=True)
        else:
            project = root / "fixture"
            shutil.copytree(Path(__file__).parent / "regex-app", project)
            (project / "Cargo.toml.in").rename(project / "Cargo.toml")
        if not (project / "Cargo.lock").is_file():
            parser.error("the benchmark needs a locked Cargo project")
        cargo_home = root / "cargo"
        cargo_home.mkdir()
        registry = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
        for name in ["registry", "git"]:
            source = registry / name
            if source.exists():
                (cargo_home / name).symlink_to(source.resolve(), target_is_directory=True)
        config = root / "empty.toml"
        config.touch()
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(("CARGO_", "KACHE_", "JATO_", "SCCACHE_", "RUSTC_", "RUSTFLAGS"))}
        env.update(CARGO=str(cargo), RUSTC=str(rustc), RUSTDOC=str(rustc.with_name("rustdoc")),
                   CARGO_HOME=str(cargo_home), CARGO_TARGET_DIR=str(target), CARGO_BUILD_BUILD_DIR=str(target),
                   CARGO_BUILD_JOBS=str(options.jobs), RUSTC_WORKSPACE_WRAPPER="", RUSTC_WRAPPER="",
                   PATH=os.pathsep.join([str(rustc.parent), "/usr/bin", "/bin", os.environ["PATH"]]),
                   CC="/usr/bin/cc", CXX="/usr/bin/c++", HOST_CC="/usr/bin/cc", HOST_CXX="/usr/bin/c++")
        if options.offline:
            env["CARGO_NET_OFFLINE"] = "true"
        run([cargo, "fetch", "--locked"], env, project)
        results = {name: {} for name in tools}
        expected = None
        order = list(tools)
        for trial in range(options.runs):
            for mode in (["check", "release"] if trial % 2 == 0 else ["release", "check"]):
                for name in order[trial % len(order):] + order[:trial % len(order)]:
                    exe = tools[name]
                    sample = {}
                    with tempfile.TemporaryDirectory(dir=root, prefix="s-") as private:
                        private = Path(private)
                        cache = private / "store"
                        cfg = dict(env, RUSTC_WRAPPER=str(exe or ""), JATO_CACHE_DIR=str(cache),
                                   JATO_MAX_SIZE="1GiB", KACHE_CACHE_DIR=str(cache),
                                   KACHE_RUNTIME_DIR=str(private / "run"), KACHE_SOCKET_PATH=str(private / "k.sock"),
                                   KACHE_CONFIG=str(config), KACHE_HOST_CONFIG="", KACHE_LOCAL_ONLY="1",
                                   KACHE_MAX_SIZE="1GiB", SCCACHE_DIR=str(cache), SCCACHE_CACHE_SIZE="1G",
                                   SCCACHE_SERVER_UDS=str(private / "s.sock"), SCCACHE_CONF=str(config),
                                   SCCACHE_ALLOW_INCREMENTAL="1")
                        assert len(os.fsencode(cfg["SCCACHE_SERVER_UDS"])) < 104, "use a shorter --scratch path"
                        command = [cargo, *(["check"] if mode == "check" else ["build", "--release"]),
                                   "--locked", "-q"]
                        def measure(phase):
                            start = time.perf_counter()
                            run(command, cfg, project)
                            sample[phase] = round(time.perf_counter() - start, 5)
                        server = {"sccache": (["--start-server"], ["--stop-server"]),
                                  "kache": (["daemon", "start"], ["daemon", "stop"])}.get(name)
                        if server:
                            run([exe, *server[0]], cfg, project)
                        try:
                            for phase in ["cold", "warm", "fresh"]:
                                if phase != "fresh" and target.exists():
                                    shutil.rmtree(target)
                                measure(phase)
                                if mode == "release" and (options.verify_bin or not options.project):
                                    args = [target / "release" / (options.verify_bin or "jato-regex-bench")]
                                    if options.verify_bin:
                                        args.append("--version")
                                    output = run(args, cfg, project)
                                    if expected is None:
                                        expected = output
                                    assert expected == output, "restored executable output changed"
                            if mode == "check" and not options.project:
                                source = project / "src/main.rs"
                                original = source.read_text()
                                source.write_text(original.replace("one two three", "one two"))
                                measure("edit")
                                source.write_text(original)
                                measure("undo")
                            if exe:
                                stats_args = {"sccache": ["--show-stats", "--stats-format", "json"],
                                              "kache": ["--json", "stats"]}.get(name, ["stats", "--json"])
                                stats = json.loads(run([exe, *stats_args], cfg, project))
                                if name == "kache":
                                    sample["hits"] = stats["lifetime"]["hits"]
                                elif name == "sccache":
                                    sample["hits"] = sum(stats["stats"]["cache_hits"]["counts"].values())
                                else:
                                    sample["hits"] = stats["counters"].get("hit", 0)
                        finally:
                            if server:
                                run([exe, *server[1]], cfg, project)
                        sample["store_bytes"] = tree_bytes(cache)
                    results[name].setdefault(mode, []).append(sample)
                    print(trial + 1, mode, name, sample, file=sys.stderr, flush=True)
        medians = {k: {m: {key: statistics.median(s[key] for s in samples)
                           for key in samples[0]} for m, samples in modes.items()}
                   for k, modes in results.items()}
        print(json.dumps(dict(versions=versions, rustc=run([rustc, "-V"], env, project).decode().strip(),
                              jobs=options.jobs, runs=options.runs, samples=results, median=medians), indent=2))


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        sys.stderr.buffer.write(error.stderr or b"")
        raise
