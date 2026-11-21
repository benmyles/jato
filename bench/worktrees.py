#!/usr/bin/env python3
"""Measure independent Cargo target directories and copied source trees on Linux."""

import argparse
import contextlib
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

from compare import binary, run, tree_bytes


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jato", default="jato")
    parser.add_argument("--previous-jato")
    parser.add_argument("--kache", default="kache")
    parser.add_argument("--sccache", default="sccache")
    parser.add_argument(
        "--tools",
        nargs="+",
        choices=["cargo", "jato", "worktree", "kache", "sccache", "sccache_basedirs"],
        default=["cargo", "jato", "worktree", "kache", "sccache", "sccache_basedirs"],
    )
    parser.add_argument(
        "--project",
        type=Path,
        help="locked workspace to copy, including relative path dependencies",
    )
    parser.add_argument(
        "--verify-bin", help="executable that implements --version, for --project"
    )
    parser.add_argument("--mode", choices=["check", "release"], default="release")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--scratch", default=os.environ.get("TMPDIR") or "/var/tmp")
    opts = parser.parse_args()
    if opts.project and opts.mode == "release" and not opts.verify_bin:
        parser.error("--project release builds need --verify-bin")
    if min(opts.runs, opts.jobs) < 1:
        parser.error("--runs and --jobs must be positive")
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
    executables = {"worktree": opts.jato, "sccache_basedirs": opts.sccache}
    tools = {
        n: cargo if n == "cargo" else binary(executables.get(n) or getattr(opts, n))
        for n in opts.tools
    }
    if opts.previous_jato:
        tools["previous_jato"] = binary(opts.previous_jato)
    versions = {
        n: subprocess.check_output([exe, "--version"], text=True).strip()
        for n, exe in tools.items()
    }
    with (
        tempfile.TemporaryDirectory(dir=opts.scratch, prefix="jw-") as temp,
        contextlib.ExitStack() as servers,
    ):
        root = Path(temp).resolve()
        first = root / "checkout"
        original = (opts.project or Path(__file__).parent / "regex-app").resolve()

        def ignore(directory, names):
            excluded = {".git", "__pycache__"}
            if Path(directory).resolve() in {original, first}:
                excluded.add("target")
            return excluded.intersection(names)

        shutil.copytree(
            original,
            first,
            symlinks=True,
            ignore=ignore,
        )
        if not opts.project:
            (first / "Cargo.toml.in").rename(first / "Cargo.toml")
        cargo_home = root / "registry-home"
        cargo_home.mkdir()
        for name in ["git", "registry"]:
            src = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo")) / name
            if src.is_dir():
                (cargo_home / name).symlink_to(src.resolve(), target_is_directory=True)
        config = root / "empty.toml"
        config.touch()
        base = {
            k: v
            for k, v in os.environ.items()
            if not k.startswith(
                ("CARGO_", "RUSTC_", "RUSTFLAGS", "JATO_", "KACHE_", "SCCACHE_")
            )
        }
        base.update(
            CARGO=str(cargo),
            RUSTC=str(rustc),
            RUSTDOC=str(rustc.with_name("rustdoc")),
            CARGO_HOME=str(cargo_home),
            CARGO_NET_OFFLINE="true",
            CARGO_BUILD_JOBS=str(opts.jobs),
            PATH=os.pathsep.join(
                [str(rustc.parent), "/usr/bin", "/bin", os.environ["PATH"]]
            ),
            CC="/usr/bin/cc",
            CXX="/usr/bin/c++",
            HOST_CC="/usr/bin/cc",
            HOST_CXX="/usr/bin/c++",
            RUSTC_WORKSPACE_WRAPPER="",
            CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="/usr/bin/cc",
        )
        # Versioned source and output roots are known before the private
        # sccache server starts and are all eligible for its prefix stripping.
        roots = [first, root / "first-target"]
        for trial in range(opts.runs):
            roots.extend(
                root / f"{name}{trial}" for name in ("checkout ", "target-", "other-")
            )
        if "sccache_basedirs" in tools:
            for path in roots:
                path.mkdir(exist_ok=True)
        configs = {}
        for n, exe in tools.items():
            private = root / n
            private.mkdir()
            store = str(private / "store")
            cfg = dict(
                base,
                RUSTC_WRAPPER="" if n in {"cargo", "worktree"} else str(exe),
                JATO_CACHE_DIR=store,
                JATO_MAX_SIZE="1GiB",
                KACHE_CACHE_DIR=store,
                KACHE_MAX_SIZE="1GiB",
                KACHE_RUNTIME_DIR=str(private / "run"),
                KACHE_SOCKET_PATH=str(private / "k.sock"),
                KACHE_CONFIG=str(config),
                KACHE_HOST_CONFIG="",
                KACHE_LOCAL_ONLY="1",
                SCCACHE_DIR=store,
                SCCACHE_CACHE_SIZE="1G",
                SCCACHE_SERVER_UDS=str(private / "s.sock"),
                SCCACHE_CONF=str(config),
                SCCACHE_ALLOW_INCREMENTAL="1",
            )
            if n == "sccache_basedirs":
                cfg["SCCACHE_BASEDIRS"] = os.pathsep.join(map(str, roots))
            configs[n] = cfg
            server = {
                "kache": (["daemon", "start"], ["daemon", "stop"]),
                "sccache": (["--start-server"], ["--stop-server"]),
            }.get("sccache" if n == "sccache_basedirs" else n)
            if server:
                run([exe, *server[0]], cfg, first)
                servers.callback(run, [exe, *server[1]], cfg, first)
        expected = None
        samples = {n: {} for n in tools}

        def measure(n, phase, project, target):
            nonlocal expected
            exe = tools[n]
            cfg = dict(
                configs[n],
                CARGO_TARGET_DIR=str(target),
                CARGO_BUILD_BUILD_DIR=str(target),
            )
            command = [exe, "worktree", "--root", "."] if n == "worktree" else [cargo]
            command.extend(
                (["check"] if opts.mode == "check" else ["build", "--release"])
                + ["--locked", "-q"]
            )
            rss = root / "rss"
            timer = Path("/usr/bin/time")
            timed = (
                [timer, "-f", "%M", "-o", rss, *command] if timer.exists() else command
            )
            baseline = hits(n, exe, cfg, project)
            start = time.perf_counter()
            run(timed, cfg, project)
            row = {"seconds": round(time.perf_counter() - start, 6)}
            if timer.exists():
                row["peak_process_kib"] = int(rss.read_text())
            if baseline is not None:
                row["hits"] = hits(n, exe, cfg, project) - baseline
            if opts.mode == "release":
                out_dir = target
                if n == "worktree":
                    paths = json.loads(
                        run([exe, "worktree", "--root", ".", "paths"], cfg, project)
                    )
                    out_dir = Path(paths["target_directory"])
                program = out_dir / "release" / (opts.verify_bin or "jato-regex-bench")
                output = run(
                    [program, *(["--version"] if opts.verify_bin else [])], cfg, project
                )
                if expected is None:
                    expected = output
                assert expected == output and (opts.project or output == b"3\n"), (
                    "incorrect build output"
                )
            samples[n].setdefault(phase, []).append(row)
            print(n, phase, row, file=sys.stderr, flush=True)

        target = root / "first-target"
        for n in tools:
            if target.exists():
                shutil.rmtree(target)
            measure(n, "cold", first, target)
            measure(n, "fresh", first, target)
        for trial in range(opts.runs):
            other = root / f"checkout {trial}"
            shutil.copytree(
                first, other, symlinks=True, ignore=ignore, dirs_exist_ok=True
            )
            names = list(tools)
            for n in names[trial % len(names) :] + names[: trial % len(names)]:
                for phase, project, dest in [
                    ("same_target", first, target),
                    ("new_target", first, root / f"target-{trial}"),
                    ("new_checkout", other, root / f"other-{trial}"),
                ]:
                    if dest.exists():
                        shutil.rmtree(dest)
                    measure(n, phase, project, dest)
            shutil.rmtree(other)
        servers.close()
        medians = {
            n: {
                phase: statistics.median(s["seconds"] for s in rows)
                for phase, rows in phases.items()
            }
            for n, phases in samples.items()
        }
        print(
            json.dumps(
                dict(
                    project=opts.project.name
                    if opts.project
                    else "locked regex application",
                    mode=opts.mode,
                    rustc=run([rustc, "-V"], base, first).decode().strip(),
                    kernel=platform.release(),
                    jobs=opts.jobs,
                    runs=opts.runs,
                    versions=versions,
                    samples=samples,
                    median_seconds=medians,
                    store_bytes={n: tree_bytes(root / n / "store") for n in tools},
                ),
                indent=2,
            )
        )


def hits(name, exe, env, project):
    if name == "sccache_basedirs":
        name = "sccache"
    if name == "cargo":
        return None
    args = {
        "kache": ["--json", "stats"],
        "sccache": ["--show-stats", "--stats-format", "json"],
    }.get(name, ["stats", "--json"])
    stats = json.loads(run([exe, *args], env, project))
    if name == "kache":
        return (
            stats["local_hits"] if "local_hits" in stats else stats["lifetime"]["hits"]
        )
    if name == "sccache":
        return sum(stats["stats"]["cache_hits"]["counts"].values())
    return stats["counters"].get("hit", 0)


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as e:
        raise SystemExit((e.stderr or b"").decode(errors="replace")) from e
