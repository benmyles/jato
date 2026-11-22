#!/usr/bin/env python3
"""Start several different builds of one project at the same moment.

Each build has its own copy of the project and its own target directory,
and compiles everything with a compiler setting of its own, so no build
can use what another one compiled: this measures how the builds share
the machine, not the cache. All the builds of one contender share that
contender's store and daemon, as they would on a developer's machine.

Reported per contender: seconds until the last build finished, processor
seconds used by all of them, the largest number of compilers running at
one moment, and the largest total resident memory of everything the
builds started (shared pages are counted once per process).
"""
import argparse
import json
import os
from pathlib import Path
import platform
import resource
import shlex
import shutil
import subprocess
import sys
import tempfile
import threading
import time

TOOLS = ["cargo", "jato", "jato-unlimited", "kache"]
PAGE = os.sysconf("SC_PAGE_SIZE")
TICK = os.sysconf("SC_CLK_TCK")


def processes():
    """Every process: its parent, name, state, resident bytes and processor seconds."""
    found = {}
    for proc in Path("/proc").glob("[0-9]*"):
        try:
            head, rest = proc.joinpath("stat").read_text().rsplit(") ", 1)
            fields = rest.split()
            found[int(proc.name)] = dict(parent=int(fields[1]), name=head.split("(", 1)[1], state=fields[0],
                                         rss=int(fields[21]) * PAGE,
                                         cpu=sum(int(fields[i]) for i in (11, 12, 13, 14)) / TICK)
        except (OSError, IndexError, ValueError):
            continue
    return found


def beneath(roots, table):
    """The processes in `table` that descend from any of `roots`, the roots included."""
    children = {}
    for pid, info in table.items():
        children.setdefault(info["parent"], []).append(pid)
    seen, pending = set(), list(roots)
    while pending:
        pid = pending.pop()
        if pid in seen or pid not in table:
            continue
        seen.add(pid)
        pending.extend(children.get(pid, []))
    return seen


def daemon_pids(private):
    needle = os.fsencode(str(private))
    found = []
    for proc in Path("/proc").glob("[0-9]*"):
        try:
            if proc.joinpath("comm").read_bytes().strip() in {b"kache-engine", b"kache"} \
                    and needle in proc.joinpath("environ").read_bytes():
                found.append(int(proc.name))
        except OSError:
            continue
    return found


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--project", type=Path, required=True, help="a Cargo project with a Cargo.lock; it is copied, never changed")
    parser.add_argument("--package", action="append", default=[])
    parser.add_argument("--verify", type=shlex.split, metavar="COMMAND",
                        help="a program under the profile directory to run from each build, with its arguments, as one word")
    parser.add_argument("--profile-dir", default="debug")
    parser.add_argument("--builds", type=int, default=4)
    parser.add_argument("--tools", nargs="+", choices=TOOLS, default=["cargo", "jato", "jato-unlimited"])
    parser.add_argument("--jato", default="jato")
    parser.add_argument("--kache", default="kache")
    parser.add_argument("--runs", type=int, default=1)
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--scratch", default=os.environ.get("TMPDIR") or "/var/tmp")
    options = parser.parse_args()

    def program(name):
        return Path(shutil.which(name) or name).resolve(strict=True)

    jato = program(options.jato) if any(t.startswith("jato") for t in options.tools) else None
    kache = program(options.kache) if "kache" in options.tools else None
    source = options.project.resolve(strict=True)
    rustc = Path(subprocess.check_output(["rustc", "--print", "sysroot"], cwd=source, text=True).strip()) / "bin/rustc"
    cargo = rustc.with_name("cargo")
    versions = {"cargo": subprocess.check_output([cargo, "-V"], text=True).strip(),
                "rustc": subprocess.check_output([rustc, "-V"], text=True).strip()}
    for name, exe in (("jato", jato), ("kache", kache)):
        if exe:
            versions[name] = subprocess.check_output([exe, "--version"], text=True).strip()
    samples = {tool: [] for tool in options.tools}

    with tempfile.TemporaryDirectory(dir=options.scratch, prefix="jcc-") as scratch:
        root = Path(scratch).resolve()
        home = root / "cargo-home"
        home.mkdir()
        real_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
        for name in ["registry", "git"]:
            if (real_home / name).exists():
                (home / name).symlink_to((real_home / name).resolve(), target_is_directory=True)
        empty = root / "empty.toml"
        empty.touch()
        base = {k: v for k, v in os.environ.items()
                if not k.startswith(("CARGO_", "KACHE_", "JATO_", "RUSTC_", "RUSTFLAGS", "RUSTUP_TOOLCHAIN"))}
        path = [p for p in os.environ["PATH"].split(os.pathsep) if "kache" not in p]
        base.update(CARGO=str(cargo), RUSTC=str(rustc), RUSTDOC=str(rustc.with_name("rustdoc")), CARGO_HOME=str(home),
                    PATH=os.pathsep.join([str(rustc.parent), *path]),
                    CC="/usr/bin/cc", CXX="/usr/bin/c++", HOST_CC="/usr/bin/cc", HOST_CXX="/usr/bin/c++")
        if options.offline:
            base["CARGO_NET_OFFLINE"] = "true"

        for trial in range(options.runs):
            order = options.tools[trial % len(options.tools):] + options.tools[:trial % len(options.tools)]
            for tool in order:
                private = root / f"{tool}-{trial}"
                private.mkdir()
                store = private / "store"
                env = dict(base, JATO_CACHE_DIR=str(store), KACHE_CACHE_DIR=str(store),
                           KACHE_RUNTIME_DIR=str(private / "run"), KACHE_SOCKET_PATH=str(private / "k.sock"),
                           KACHE_CONFIG=str(empty), KACHE_HOST_CONFIG="", KACHE_LOCAL_ONLY="1", KACHE_MAX_SIZE="40GiB")
                front = [str(cargo)]
                if tool.startswith("jato"):
                    front = [str(jato)]
                    if tool == "jato-unlimited":
                        env["JATO_JOBS"] = "0"
                elif tool == "kache":
                    env["RUSTC_WRAPPER"] = str(kache)
                    subprocess.run([kache, "daemon", "start"], env=env, cwd=source, check=True, capture_output=True)
                try:
                    builds = []
                    for n in range(options.builds):
                        project = private / f"checkout-{n}"
                        subprocess.run(["cp", "-a", "--reflink=auto", source, project], check=True)
                        shutil.rmtree(project / "target", ignore_errors=True)
                        # A setting no crate looks at, which makes every compilation this build's own.
                        setting = f"target.'cfg(all())'.rustflags=[\"--cfg=jato_bench_build_{trial}_{n}\"]"
                        command = [*front, "build", "--locked", "--config", setting,
                                   *[a for p in options.package for a in ("-p", p)]]
                        builds.append((project, command))
                    watched = daemon_pids(private)
                    served = sum(processes().get(pid, {}).get("cpu", 0) for pid in watched)
                    before = resource.getrusage(resource.RUSAGE_CHILDREN)
                    load = os.getloadavg()[0]
                    start = time.perf_counter()
                    running = [subprocess.Popen(command, cwd=project, env=dict(env, CARGO_TARGET_DIR=str(project / "target")),
                                                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
                               for project, command in builds]
                    peak = dict(rss=0, compilers=0, runnable=0)
                    done = threading.Event()

                    def watch():
                        roots = [p.pid for p in running] + watched
                        while not done.wait(0.2):
                            table = processes()
                            ours = beneath(roots, table)
                            peak["rss"] = max(peak["rss"], sum(table[pid]["rss"] for pid in ours))
                            compilers = [pid for pid in ours if table[pid]["name"] == "rustc"]
                            peak["compilers"] = max(peak["compilers"], len(compilers))
                            peak["runnable"] = max(peak["runnable"], sum(table[pid]["state"] == "R" for pid in ours))

                    watcher = threading.Thread(target=watch, daemon=True)
                    watcher.start()
                    finished = []
                    for proc in running:
                        errors = proc.stderr.read()
                        if proc.wait():
                            sys.stderr.buffer.write(errors[-4000:])
                            raise SystemExit(f"a {tool} build failed")
                        finished.append(time.perf_counter() - start)
                    elapsed = time.perf_counter() - start
                    done.set()
                    watcher.join()
                    after = resource.getrusage(resource.RUSAGE_CHILDREN)
                    cpu = (after.ru_utime - before.ru_utime) + (after.ru_stime - before.ru_stime)
                    cpu += sum(processes().get(pid, {}).get("cpu", 0) for pid in watched) - served
                    if options.verify:
                        for project, _ in builds:
                            subprocess.run([project / "target" / options.profile_dir / options.verify[0], *options.verify[1:]],
                                           cwd=project, check=True, capture_output=True)
                    sample = dict(seconds=round(elapsed, 2), each=[round(t, 2) for t in sorted(finished)], cpu=round(cpu, 1),
                                  peak_compilers=peak["compilers"], peak_runnable=peak["runnable"],
                                  peak_rss_mib=round(peak["rss"] / 2**20), load_before=round(load, 1))
                    if tool.startswith("jato"):
                        stats = json.loads(subprocess.check_output([jato, "stats", "--json"], env=env, cwd=source))
                        sample["queued"] = stats["counters"].get("queued", 0)
                        sample["queued_seconds"] = round(stats["counters"].get("queued_ms", 0) / 1000, 1)
                    samples[tool].append(sample)
                    print(trial + 1, tool, sample, file=sys.stderr, flush=True)
                finally:
                    if tool == "kache":
                        subprocess.run([kache, "daemon", "stop"], env=env, cwd=source, capture_output=True)
                shutil.rmtree(private, ignore_errors=True)

    print(json.dumps(dict(project=source.name, packages=options.package, builds=options.builds, versions=versions,
                          processors=os.cpu_count(), kernel=platform.release(), runs=options.runs, samples=samples), indent=2))


if __name__ == "__main__":
    main()
