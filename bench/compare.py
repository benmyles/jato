#!/usr/bin/env python3
"""Compare ways of building one Cargo project: bare Cargo, Jato, kache and sccache.

Every contender gets its own empty store, its own daemon where it has one,
its own copy of the project and its own target directories, and is taken
through the same steps:

  cold        nothing stored, nothing built
  fresh       the same build again
  restore     the target directory removed
  new_target  a target directory that has never been used
  copy        the same sources at another path, building into ./target there
  copy_edit   one source file changed in that copy, where nothing was compiled
  edit_first  one source file changed, in the target that was restored
  edit        the median of further changes to that file
  undo        the file as it was before the changes

The built program is run after each step and must print what it printed
for the first contender. Times are wall-clock seconds; cpu is user plus
system time of everything the step started, including a daemon's share.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import resource
import shlex
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

TOOLS = ["cargo", "jato", "jato-wrapper", "kache", "kache-shim", "sccache"]
STEPS = ["cold", "fresh", "restore", "new_target", "copy", "copy_edit", "edit_first", "edit", "undo"]
TICK = os.sysconf("SC_CLK_TCK")


def daemons(private):
    """Processes that serve this contender: started with its private directory in their environment."""
    needle = os.fsencode(str(private))
    found = []
    for proc in Path("/proc").glob("[0-9]*"):
        try:
            if proc.joinpath("comm").read_bytes().strip() not in {b"sccache", b"kache-engine", b"kache"}:
                continue
            if needle in proc.joinpath("environ").read_bytes():
                found.append(proc)
        except OSError:
            continue
    return found


def daemon_cpu(private):
    total = 0.0
    for proc in daemons(private):
        try:
            fields = proc.joinpath("stat").read_text().rsplit(") ", 1)[1].split()
            # utime, stime, cutime, cstime: its own and that of the compilers it ran.
            total += sum(int(fields[i]) for i in (11, 12, 13, 14)) / TICK
        except (OSError, IndexError, ValueError):
            continue
    return total


def daemon_rss(private):
    total = 0
    for proc in daemons(private):
        try:
            line = next(l for l in proc.joinpath("status").read_text().splitlines() if l.startswith("VmRSS:"))
            total += int(line.split()[1])
        except (OSError, StopIteration):
            continue
    return total


def tree_bytes(root):
    seen, total = set(), 0
    for directory, _, files in os.walk(root):
        for name in files:
            try:
                info = (Path(directory) / name).lstat()
            except OSError:
                continue
            if (info.st_dev, info.st_ino) not in seen:
                seen.add((info.st_dev, info.st_ino))
                total += info.st_size
    return total


def extents(root):
    """Bytes of `root` that nothing else on a Btrfs filesystem shares, and bytes that something does."""
    if not root.is_dir() or not shutil.which("btrfs"):
        return None
    subprocess.run(["sync", "-f", root], check=False)
    out = subprocess.run(["btrfs", "filesystem", "du", "-s", "--raw", root], capture_output=True, text=True)
    try:
        total, exclusive, shared = map(int, out.stdout.splitlines()[-1].split()[:3])
    except (ValueError, IndexError):
        return None
    return dict(total=total, exclusive=exclusive, shared=shared)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--project", type=Path, required=True, help="a Cargo project with a Cargo.lock; it is copied, never changed")
    parser.add_argument("--package", action="append", default=[], help="build this workspace package (repeatable)")
    parser.add_argument("--command", choices=["build", "check"], default="build", help="the Cargo command each contender runs")
    parser.add_argument("--c-compilers", choices=["alone", "own"], default="alone",
                        help="alone: build scripts are given the C compilers by their paths, and no contender caches C. "
                             "own: each contender handles C its own way, kache as `kache cc`, sccache as `sccache cc`, "
                             "Jato through the names it puts on PATH")
    parser.add_argument("--cargo-args", nargs=argparse.REMAINDER, default=[], help="further arguments for the Cargo command, last on the line")
    parser.add_argument("--edit", required=True, help="a source file, relative to the project, to append a line to")
    parser.add_argument("--verify", type=shlex.split, metavar="COMMAND",
                        help="a program under the profile directory to run after each step, with its arguments, as one word")
    parser.add_argument("--profile-dir", default="debug")
    parser.add_argument("--tools", nargs="+", choices=TOOLS, default=["cargo", "jato", "kache"])
    parser.add_argument("--jato", default="jato")
    parser.add_argument("--kache", default="kache")
    parser.add_argument("--sccache", default="sccache")
    parser.add_argument("--no-rebuilds", nargs="+", choices=TOOLS, default=[], metavar="TOOL",
                        help="leave out restore, new_target and copy for these contenders, for which each is a cold build again")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--edits", type=int, default=3)
    parser.add_argument("--jobs", type=int, help="Cargo jobs; default is Cargo's")
    parser.add_argument("--cache-gib", type=int, default=20)
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--scratch", default=os.environ.get("TMPDIR") or "/var/tmp", help="a directory on disk, not in memory")
    options = parser.parse_args()

    def program(name):
        return Path(shutil.which(name) or name).resolve(strict=True)

    programs = {"jato": program(options.jato) if any(t.startswith("jato") for t in options.tools) else None,
                "kache": program(options.kache) if any(t.startswith("kache") for t in options.tools) else None,
                "sccache": program(options.sccache) if "sccache" in options.tools else None}
    source = options.project.resolve(strict=True)
    if not (source / "Cargo.lock").is_file():
        parser.error("the project needs a Cargo.lock")
    # The toolchain the project selects, found once so that no contender pays for rustup.
    rustc = Path(subprocess.check_output(["rustc", "--print", "sysroot"], cwd=source, text=True).strip()) / "bin/rustc"
    cargo = rustc.with_name("cargo")
    versions = {"cargo": subprocess.check_output([cargo, "-V"], text=True).strip(),
                "rustc": subprocess.check_output([rustc, "-V"], text=True).strip()}
    for name, exe in programs.items():
        if exe:
            versions[name] = subprocess.check_output([exe, "--version"], text=True).strip()
    build = [options.command, "--locked", *[a for p in options.package for a in ("-p", p)], *options.cargo_args]
    if options.jobs:
        build += ["-j", str(options.jobs)]
    samples = {tool: {step: [] for step in STEPS} for tool in options.tools}
    details = {tool: [] for tool in options.tools}
    expected = None

    with tempfile.TemporaryDirectory(dir=options.scratch, prefix="jc-") as scratch:
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
                if not k.startswith(("CARGO_", "KACHE_", "JATO_", "SCCACHE_", "RUSTC_", "RUSTFLAGS", "RUSTUP_TOOLCHAIN"))}
        path = [p for p in os.environ["PATH"].split(os.pathsep) if "kache" not in p and "sccache" not in p]
        base.update(CARGO=str(cargo), RUSTC=str(rustc), RUSTDOC=str(rustc.with_name("rustdoc")), CARGO_HOME=str(home),
                    PATH=os.pathsep.join([str(rustc.parent), *path]),
                    CC="/usr/bin/cc", CXX="/usr/bin/c++", HOST_CC="/usr/bin/cc", HOST_CXX="/usr/bin/c++")
        if options.offline:
            base["CARGO_NET_OFFLINE"] = "true"

        def copy_project(to):
            subprocess.run(["cp", "-a", "--reflink=auto", source, to], check=True)
            shutil.rmtree(to / "target", ignore_errors=True)

        for trial in range(options.runs):
            order = options.tools[trial % len(options.tools):] + options.tools[:trial % len(options.tools)]
            for tool in order:
                private = root / f"{tool}-{trial}"
                private.mkdir()
                project, copy = private / "project", private / "elsewhere" / "project"
                copy_project(project)
                store = private / "store"
                env = dict(base, JATO_CACHE_DIR=str(store), JATO_MAX_SIZE=f"{options.cache_gib}GiB",
                           KACHE_CACHE_DIR=str(store), KACHE_RUNTIME_DIR=str(private / "run"),
                           KACHE_SOCKET_PATH=str(private / "k.sock"), KACHE_CONFIG=str(empty), KACHE_HOST_CONFIG="",
                           KACHE_LOCAL_ONLY="1", KACHE_MAX_SIZE=f"{options.cache_gib}GiB",
                           SCCACHE_DIR=str(store), SCCACHE_CACHE_SIZE=f"{options.cache_gib}G",
                           SCCACHE_SERVER_UDS=str(private / "s.sock"), SCCACHE_CONF=str(empty))
                kind = tool.split("-")[0]
                exe = programs.get(kind)
                if options.c_compilers == "own":
                    for name in ["CC", "CXX", "HOST_CC", "HOST_CXX"]:
                        compiler = "c++" if name.endswith("CXX") else "cc"
                        if kind in ("kache", "sccache"):
                            # As their setups leave it for build scripts to find.
                            env[name] = f"{exe} {compiler}"
                        else:
                            del env[name]
                    if kind in ("kache", "sccache"):
                        env["CC_KNOWN_WRAPPER_CUSTOM"] = kind
                command = [str(cargo), *build]
                if tool == "jato":
                    command = [str(exe), *build]
                elif tool == "kache-shim":
                    shim = private / "cargo"
                    shim.symlink_to(exe)
                    env["KACHE_REAL_CARGO"] = str(cargo)
                    command = [str(shim), *build]
                elif exe:
                    env["RUSTC_WRAPPER"] = str(exe)
                server = {"sccache": (["--start-server"], ["--stop-server"]),
                          "kache": (["daemon", "start"], ["daemon", "stop"])}.get(kind)
                detail = {}

                def step(name, where, target):
                    """Time one build of the project at `where` into `target`, then run what it built."""
                    cfg = dict(env, CARGO_TARGET_DIR=str(target))
                    before = resource.getrusage(resource.RUSAGE_CHILDREN)
                    served = daemon_cpu(private)
                    load = os.getloadavg()[0]
                    start = time.perf_counter()
                    done = subprocess.run(command, cwd=where, env=cfg, capture_output=True)
                    elapsed = time.perf_counter() - start
                    after = resource.getrusage(resource.RUSAGE_CHILDREN)
                    if done.returncode:
                        sys.stderr.buffer.write(done.stderr[-4000:])
                        raise SystemExit(f"{tool} failed at {name}")
                    cpu = (after.ru_utime - before.ru_utime) + (after.ru_stime - before.ru_stime) + daemon_cpu(private) - served
                    sample = dict(seconds=round(elapsed, 3), cpu=round(cpu, 2), load=round(load, 1),
                                  compiled=done.stderr.count(b"Compiling "))
                    if options.verify:
                        nonlocal expected
                        ran = subprocess.run([target / options.profile_dir / options.verify[0], *options.verify[1:]],
                                             cwd=where, env=cfg, capture_output=True)
                        printed = (ran.returncode, ran.stdout)
                        if expected is None:
                            expected = printed
                        if printed != expected:
                            raise SystemExit(f"{tool} at {name}: the built program printed something else")
                    return sample

                if server:
                    subprocess.run([exe, *server[0]], env=env, cwd=project, check=True, capture_output=True)
                try:
                    # Where Cargo builds by default, and somewhere it has not been.
                    one, two = project / "target", private / "target-two"
                    record = lambda name, sample: (samples[tool][name].append(sample), print(trial + 1, tool, name, sample, file=sys.stderr, flush=True))
                    record("cold", step("cold", project, one))
                    record("fresh", step("fresh", project, one))
                    detail["store_bytes"] = tree_bytes(store)
                    detail["store_extents"] = extents(store)
                    detail["target_bytes"] = tree_bytes(one)
                    detail["target_extents"] = extents(one)
                    if tool not in options.no_rebuilds:
                        shutil.rmtree(one)
                        record("restore", step("restore", project, one))
                        record("new_target", step("new_target", project, two))
                        copy.parent.mkdir()
                        copy_project(copy)
                        record("copy", step("copy", copy, copy / "target"))
                        changed = copy / options.edit
                        changed.write_bytes(changed.read_bytes() + f"\npub fn changed_in_the_copy_{trial}() -> u64 {{ {time.time_ns()} }}\n".encode())
                        record("copy_edit", step("copy_edit", copy, copy / "target"))
                    edited = project / options.edit
                    original = edited.read_bytes()
                    later = []
                    for n in range(options.edits + 1):
                        edited.write_bytes(original + f"\npub fn changed_{trial}_{n}() -> u64 {{ {time.time_ns()} }}\n".encode())
                        sample = step("edit", project, one)
                        if n == 0:
                            record("edit_first", sample)
                        else:
                            later.append(sample)
                    middle = sorted(later, key=lambda s: s["seconds"])[len(later) // 2]
                    record("edit", middle)
                    edited.write_bytes(original)
                    record("undo", step("undo", project, one))
                    detail["daemon_rss_kib"] = daemon_rss(private)
                    detail["store_bytes_end"] = tree_bytes(store)
                    if kind == "jato":
                        stats = json.loads(subprocess.check_output([exe, "stats", "--json"], env=env, cwd=project))
                        detail["counters"] = stats["counters"]
                finally:
                    if server:
                        subprocess.run([exe, *server[1]], env=env, cwd=project, capture_output=True)
                details[tool].append(detail)
                shutil.rmtree(private, ignore_errors=True)

    median = {tool: {step: {key: statistics.median(s[key] for s in rows) for key in ("seconds", "cpu")}
                     for step, rows in steps.items() if rows} for tool, steps in samples.items()}
    filesystem = subprocess.run(["findmnt", "-no", "FSTYPE", "-T", options.scratch], capture_output=True, text=True).stdout.strip()
    print(json.dumps(dict(project=source.name, command=options.command, c_compilers=options.c_compilers,
                          packages=options.package, cargo_args=options.cargo_args, edit=options.edit,
                          versions=versions, runs=options.runs, jobs=options.jobs, processors=os.cpu_count(),
                          kernel=platform.release(), filesystem=filesystem, median=median, samples=samples,
                          details=details), indent=2))
    width = max(len(t) for t in options.tools) + 2
    print("".ljust(12) + "".join(t.rjust(width) for t in options.tools), file=sys.stderr)
    for name in STEPS:
        cells = [f"{median[t][name]['seconds']:.2f}" if name in median[t] else "-" for t in options.tools]
        print(name.ljust(12) + "".join(c.rjust(width) for c in cells), file=sys.stderr)


if __name__ == "__main__":
    main()
