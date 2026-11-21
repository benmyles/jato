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
        rows = (line.split(None, 7) for line in Path("/proc/net/unix").read_bytes().splitlines())
        inodes = {row[6].decode("ascii") for row in rows if len(row) == 8 and row[7] == os.fsencode(sock)}
        wanted = {f"socket:[{inode}]" for inode in inodes}
        for proc in Path("/proc").glob("[0-9]*"):
            try:
                if proc.joinpath("comm").read_bytes().strip() not in {b"sccache", b"kache-engine", b"kache"}:
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


def build_command(cargo, mode, jato=None):
    if mode == "jato-dev":
        if jato:
            return [jato, "dev", "build", "--locked", "-q"]
        # Give every peer the same Cargo profile used by the measured Jato CLI.
        cfg = []
        for setting in ['profile.jato-dev.inherits="dev"', 'profile.jato-dev.codegen-units=16',
                        'profile.jato-dev.debug="line-tables-only"', 'profile.jato-dev.package."*".debug="none"']:
            cfg.extend(["--config", setting])
        return [cargo, *cfg, "build", "--locked", "-q", "--profile", "jato-dev"]
    args = {"check": ["check"], "release": ["build", "--release"], "dev": ["build"]}
    return [cargo, *args[mode], "--locked", "-q"]


def profile_directory(mode):
    return "debug" if mode == "dev" else mode


def paired_restores(options, tools, env, project, root, target):
    samples = {}
    medians = {}
    for mode in options.workloads:
        timings = {name: [] for name in tools}
        expected = None
        names = list(tools)
        # One untimed build per store, followed by alternating clean targets.
        for iteration in range(options.paired_restores + 1):
            for name in names[iteration % len(names):] + names[:iteration % len(names)]:
                cfg = dict(env, RUSTC_WRAPPER=str(tools[name]), JATO_CACHE_DIR=str(root / f"{name}-{mode}"))
                if name.endswith("_packed"):
                    cfg["JATO_STORAGE"] = "packed"
                if target.exists():
                    shutil.rmtree(target)
                start = time.perf_counter()
                run(build_command(env["CARGO"], mode, tools[name] if name in {"jato", "jato_packed"} else None), cfg, project)
                elapsed = time.perf_counter() - start
                if iteration:
                    timings[name].append(round(elapsed, 6))
                if mode != "check" and (options.verify_bin or not options.project):
                    output = run([target / profile_directory(mode) / (options.verify_bin or f"jato-{options.fixture}-bench"),
                                  *(["--version"] if options.verify_bin else [])], cfg, project)
                    if expected is None:
                        expected = output
                    assert expected == output, "restored program changed"
        samples[mode] = timings
        medians[mode] = {name: statistics.median(rows) for name, rows in timings.items()}
    return dict(samples=samples, median=medians)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group()
    source.add_argument("--project", type=Path, help="an existing locked Cargo project")
    source.add_argument("--fixture", choices=["regex", "zlib", "embedded"], default="regex")
    parser.add_argument("--verify-bin", help="compare this release program's --version output")
    parser.add_argument("--jato", default="jato")
    parser.add_argument("--previous-jato")
    parser.add_argument("--compiler-path", choices=["absolute", "path", "rustup"], default="absolute",
                        help="give Cargo an absolute rustc, a rustc on PATH, or a rustup proxy on PATH")
    parser.add_argument("--include-packed", action="store_true", help="also measure Jato with portable packed storage")
    parser.add_argument("--previous-packed", action="store_true", help="also measure --previous-jato in packed mode")
    parser.add_argument("--paired-restores", type=int, metavar="N",
                        help="prime the two Jato versions and alternate N warm clean targets per version")
    parser.add_argument("--workloads", nargs="+", choices=["release","check","dev","jato-dev"], default=["check","release"])
    parser.add_argument("--kache", default="kache")
    parser.add_argument("--local-kache", help="also compare a second installed kache with its own daemon")
    parser.add_argument("--sccache", default="sccache")
    parser.add_argument("--tools", nargs="+", choices=["cargo","jato","kache","sccache"],
                        default=["cargo","jato","kache","sccache"], help="base contenders to measure")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--scratch", default=os.environ.get("TMPDIR") or "/var/tmp")
    options = parser.parse_args()
    if min(options.jobs, options.runs) < 1:
        parser.error("--jobs and --runs must be positive")
    if options.paired_restores is not None:
        if options.paired_restores < 1 or not options.previous_jato:
            parser.error("--paired-restores needs a positive count and --previous-jato")
        tools = {"jato": binary(options.jato)}
    else:
        tools = {name: None if name=="cargo" else binary(getattr(options,name)) for name in options.tools}
        if options.local_kache:
            tools["kache_local"] = binary(options.local_kache)
    if options.include_packed:
        tools["jato_packed"] = binary(options.jato)
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
        if options.compiler_path != "absolute":
            env.pop("RUSTC")
        if options.compiler_path == "rustup":
            proxies = root / "proxies"
            proxies.mkdir()
            for name in ["rustc", "rustup"]:
                (proxies / name).symlink_to(binary("rustup"))
            env["PATH"] = os.pathsep.join([str(proxies), env["PATH"]])
        if options.offline:
            env["CARGO_NET_OFFLINE"] = "true"
        run([cargo, "fetch", "--locked"], env, project)
        if options.paired_restores:
            result = paired_restores(options, tools, env, project, root, target)
            print(json.dumps(dict(versions=versions, compiler_path=options.compiler_path, project=project.name if options.project else options.fixture,
                                  rustc=run([rustc, "-V"], env, project).decode().strip(), jobs=options.jobs,
                                  pairs=options.paired_restores, **result), indent=2))
            return
        results = {name: {} for name in tools}
        expected = None
        order = list(tools)
        for trial in range(options.runs):
            for mode in (options.workloads if trial % 2 == 0 else reversed(options.workloads)):
                for name in order[trial % len(order):] + order[:trial % len(order)]:
                    exe = tools[name]
                    kind = "kache" if name=="kache_local" else name
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
                        command = build_command(cargo, mode, exe if name in {"jato", "jato_packed"} else None)
                        def measure(phase):
                            rss = private / "rss"
                            timed = ["/usr/bin/time", "-f", "%M %U %S", "-o", rss, *command] if Path("/usr/bin/time").is_file() else command
                            start = time.perf_counter()
                            run(timed, cfg, project)
                            sample[phase] = round(time.perf_counter() - start, 5)
                            if rss.is_file():
                                peak, user, system = rss.read_text().split()
                                sample[phase + "_peak_process_rss_kib"] = int(peak)
                                # Private peer daemons launch compilers outside
                                # Cargo's process tree. Its CPU sum is complete
                                # for bare Cargo and both daemonless Jato runs.
                                if kind not in {"kache", "sccache"}:
                                    sample[phase + "_cpu_seconds"] = round(float(user) + float(system), 2)
                        server = {"sccache": (["--start-server"], ["--stop-server"]),
                                  "kache": (["daemon", "start"], ["daemon", "stop"])}.get(kind)
                        if server:
                            run([exe, *server[0]], cfg, project)
                        try:
                            for phase in ["cold", "warm", "fresh"]:
                                if phase != "fresh" and target.exists():
                                    shutil.rmtree(target)
                                measure(phase)
                                if mode != "check" and (options.verify_bin or not options.project):
                                    args = [target / profile_directory(mode) / (options.verify_bin or f"jato-{options.fixture}-bench")]
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
                                              "kache": ["--json", "stats"]}.get(kind, ["stats", "--json"])
                                stats = json.loads(run([exe, *stats_args], cfg, project))
                                if server:
                                    socket_path = cfg["KACHE_SOCKET_PATH" if kind == "kache" else "SCCACHE_SERVER_UDS"]
                                    sample["idle_server_rss_kib"] = server_rss(socket_path)
                                else:
                                    sample["idle_server_rss_kib"] = 0
                                if name in {"jato","jato_packed","previous_jato","previous_packed"}:
                                    sample["sealed_bytes"] = stats.get("sealed_bytes", 0)
                                    sample["packed_bytes"] = stats["compressed_bytes"]
                                    sample["probe_hits"] = stats["counters"].get("probe_hit", 0)
                                if kind == "kache":
                                    # 2.2 exposes local hits directly. Each
                                    # trial uses a private, local-only store.
                                    sample["hits"] = stats["local_hits"] if "local_hits" in stats else stats["lifetime"]["hits"]
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
        print(json.dumps(dict(versions=versions, compiler_path=options.compiler_path, kernel=platform.release(), project=project.name if options.project else f"locked {options.fixture} application",
                              rustc=run([rustc, "-V"], env, project).decode().strip(),
                              jobs=options.jobs, runs=options.runs, samples=results, median=medians), indent=2))


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        sys.stderr.buffer.write(error.stderr or b"")
        raise
