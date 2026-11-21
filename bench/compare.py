#!/usr/bin/env python3
"""Compare local Rust compiler wrappers using identical Cargo arguments and target paths."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
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
                info = (Path(directory) / name).lstat()
            except FileNotFoundError:
                continue
            identity = info.st_dev, info.st_ino
            if identity not in seen:
                total += info.st_size
                seen.add(identity)
    return total


def server_rss(sock):
    """Identify only the server listening on this benchmark's private socket."""
    try:
        rows = Path("/proc/net/unix").read_text().splitlines()
        inodes = {row.split()[6] for row in rows if row.split()[-1] == str(sock)}
        wanted = {f"socket:[{inode}]" for inode in inodes}
        for proc in Path("/proc").glob("[0-9]*"):
            try:
                if proc.joinpath("comm").read_text().strip() not in {"sccache", "kache-engine", "kache"}:
                    continue
                if any(os.readlink(fd) in wanted for fd in proc.joinpath("fd").iterdir()):
                    rss = next(s for s in proc.joinpath("status").read_text().splitlines() if s.startswith("VmRSS:"))
                    return int(rss.split()[1])
            except (FileNotFoundError, ProcessLookupError, PermissionError):
                continue
    except (OSError, StopIteration):
        pass
    return None


def extent_usage(root):
    if not root.is_dir() or not shutil.which("btrfs"):
        return {}
    # Only flush files owned by this trial, after timing. Delayed allocation
    # can otherwise give a small new store an apparent zero extent footprint.
    for directory, _, files in os.walk(root):
        for name in files:
            path = Path(directory) / name
            if path.is_file() and not path.is_symlink():
                with path.open("rb") as stream:
                    os.fsync(stream.fileno())
    result = subprocess.run(["btrfs", "filesystem", "du", "-s", "--raw", root], capture_output=True, text=True)
    if result.returncode:
        return {}
    try:
        total, exclusive, shared = map(int, result.stdout.splitlines()[-1].split()[:3])
    except (ValueError, IndexError):
        return {}
    return dict(btrfs_total=total, btrfs_exclusive=exclusive, btrfs_set_shared=shared)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group()
    source.add_argument("--project", type=Path, help="an existing locked Cargo project")
    source.add_argument("--fixture", choices=["regex", "zlib", "embedded"], default="regex")
    parser.add_argument("--verify-bin", help="compare this release program's --version output")
    parser.add_argument("--jato", default="jato")
    parser.add_argument("--previous-jato")
    parser.add_argument("--include-packed", action="store_true", help="also measure Jato with portable packed storage")
    parser.add_argument("--previous-packed", action="store_true", help="also measure --previous-jato in packed mode")
    parser.add_argument("--workloads", nargs="+", choices=["release","check"], default=["check","release"])
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
    if options.include_packed:
        tools["jato_packed"] = tools["jato"]
    if options.previous_jato:
        tools["previous_jato"] = binary(options.previous_jato)
    if options.previous_packed:
        if not options.previous_jato:
            parser.error("--previous-packed needs --previous-jato")
        tools["previous_packed"] = tools["previous_jato"]
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
            shutil.copytree(Path(__file__).parent / (options.fixture + "-app"), project)
            (project / "Cargo.toml.in").rename(project / "Cargo.toml")
            known_output, edit_from, edit_to = {
                "regex": (b"3\n", "one two three", "one two"),
                "zlib": (b"verified native library round trip\n", "native library", "native archive"),
                "embedded": (None, "unwrap_or(0)", "unwrap_or(1)"),
            }[options.fixture]
            if options.fixture == "embedded":
                payload = hashlib.shake_256(b"jato embedded fixture v1").digest(12 * 1024 * 1024 + 17)
                (project / "asset.bin").write_bytes(payload)
                known_output = f"{len(payload)} {payload[0]}\n".encode()
                del payload
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
            for mode in (options.workloads if trial % 2 == 0 else reversed(options.workloads)):
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
                        if name in {"jato_packed", "previous_packed"}:
                            cfg["JATO_STORAGE"] = "packed"
                        assert len(os.fsencode(cfg["SCCACHE_SERVER_UDS"])) < 104, "use a shorter --scratch path"
                        command = [cargo, *(["check"] if mode == "check" else ["build", "--release"]),
                                   "--locked", "-q"]
                        def measure(phase):
                            rss = private / "rss"
                            timed = ["/usr/bin/time", "-f", "%M", "-o", rss, *command] if Path("/usr/bin/time").is_file() else command
                            start = time.perf_counter()
                            run(timed, cfg, project)
                            sample[phase] = round(time.perf_counter() - start, 5)
                            if rss.is_file():
                                sample[phase + "_peak_process_rss_kib"] = int(rss.read_text())
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
                                    args = [target / "release" / (options.verify_bin or f"jato-{options.fixture}-bench")]
                                    if options.verify_bin:
                                        args.append("--version")
                                    output = run(args, cfg, project)
                                    if expected is None:
                                        expected = output
                                    assert expected == output, "restored executable output changed"
                                    if not options.project:
                                        assert output == known_output, "incorrect fixture result"
                            if mode == "check" and not options.project:
                                source = project / "src/main.rs"
                                original = source.read_text()
                                assert edit_from in original, "fixture edit no longer applies"
                                source.write_text(original.replace(edit_from, edit_to, 1))
                                measure("edit")
                                source.write_text(original)
                                measure("undo")
                            if exe:
                                stats_args = {"sccache": ["--show-stats", "--stats-format", "json"],
                                              "kache": ["--json", "stats"]}.get(name, ["stats", "--json"])
                                stats = json.loads(run([exe, *stats_args], cfg, project))
                                if server:
                                    socket_path = cfg["KACHE_SOCKET_PATH" if name == "kache" else "SCCACHE_SERVER_UDS"]
                                    sample["idle_server_rss_kib"] = server_rss(socket_path)
                                else:
                                    sample["idle_server_rss_kib"] = 0
                                if name in {"jato","jato_packed","previous_jato","previous_packed"}:
                                    sample["sealed_bytes"] = stats.get("sealed_bytes", 0)
                                    sample["packed_bytes"] = stats["compressed_bytes"]
                                    sample["probe_hits"] = stats["counters"].get("probe_hit", 0)
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
                        sample.update(extent_usage(cache))
                    results[name].setdefault(mode, []).append(sample)
                    print(trial + 1, mode, name, sample, file=sys.stderr, flush=True)
        medians = {k: {m: {key: statistics.median(s[key] for s in samples)
                           for key in samples[0] if all(s.get(key) is not None for s in samples)} for m, samples in modes.items()}
                   for k, modes in results.items()}
        print(json.dumps(dict(versions=versions, kernel=platform.release(), project=project.name if options.project else f"locked {options.fixture} application",
                              rustc=run([rustc, "-V"], env, project).decode().strip(),
                              jobs=options.jobs, runs=options.runs, samples=results, median=medians), indent=2))


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        sys.stderr.buffer.write(error.stderr or b"")
        raise
