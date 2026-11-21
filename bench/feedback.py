#!/usr/bin/env python3
"""Measure a save to a real compiler diagnostic and the subsequent fix."""
import argparse
import json
import os
from pathlib import Path
from queue import Empty, Queue
import shutil
import signal
import statistics
import subprocess
import tempfile
from threading import Thread
import time


def idle_tree_pss(pid):
    """Account for shared code pages only once across the idle view and watcher."""
    pending = [pid]
    total = 0
    try:
        while pending:
            proc = Path("/proc") / str(pending.pop())
            rollup = (proc / "smaps_rollup").read_text().splitlines()
            total += int(next(line.split()[1] for line in rollup if line.startswith("Pss:")))
            for children in (proc / "task").glob("*/children"):
                pending.extend(map(int, children.read_text().split()))
    except (OSError, StopIteration):
        return None
    return total


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jato", default="jato")
    parser.add_argument("--dev", action="store_true", help="use the jato-dev profile for the watcher and initial check")
    parser.add_argument("--worktree", action="store_true", help="run the watcher inside a projected checkout")
    parser.add_argument("--directories", type=int, default=0, help="add this many watched leaf directories and measure attaching new subtrees")
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--debounce", type=int, help="override the installed watcher’s default")
    parser.add_argument("--scratch", default=os.environ.get("TMPDIR") or "/var/tmp")
    options = parser.parse_args()
    if options.directories < 0:
        parser.error("--directories must be nonnegative")
    if options.runs < 1:
        parser.error("--runs must be positive")
    jato = Path(shutil.which(options.jato) or options.jato).resolve(strict=True)
    with tempfile.TemporaryDirectory(dir=options.scratch, prefix="jato-feedback-") as scratch:
        root = Path(scratch).resolve()
        workspace = root / "workspace"
        workspace.mkdir()
        (workspace / "Cargo.toml").write_text('[workspace]\nmembers=["parser","client"]\nresolver="2"\n')
        for name in ["parser", "client"]:
            package = workspace / name
            (package / "src").mkdir(parents=True)
            manifest = f'[package]\nname="{name}"\nversion="0.1.0"\nedition="2021"\n'
            if name == "client":
                manifest += '[dependencies]\nparser={path="../parser"}\n'
            (package / "Cargo.toml").write_text(manifest)
        for i in range(options.directories):
            (workspace / "parser/assets" / str(i // 64) / str(i)).mkdir(parents=True)
        source = workspace / "parser/src/lib.rs"
        good = "pub fn parse() -> u8 { 42 }"
        source.write_text(good)
        (workspace / "client/src/lib.rs").write_text("pub fn run() -> u8 { parser::parse() }")
        env = dict(os.environ, JATO_CACHE_DIR=str(root / "cache"), CARGO_HOME=str(root / "cargo"),
                   CARGO_TARGET_DIR=str(root / "target"), CARGO_BUILD_BUILD_DIR=str(root / "target"),
                   CARGO_NET_OFFLINE="true", RUSTC_WRAPPER="", RUSTC_WORKSPACE_WRAPPER="")
        command = [jato, "worktree", "--root", "."] if options.worktree else [jato]
        if options.dev:
            command.append("dev")
        subprocess.run([*command, "check", "--workspace", "-q"], cwd=workspace, env=env, check=True)
        watch_args = [] if options.debounce is None else ["--debounce", str(options.debounce)]
        launched = time.perf_counter()
        proc = subprocess.Popen([*command, "watch", *watch_args], cwd=workspace,
                                env=env, stderr=subprocess.PIPE, text=True)
        events = Queue()

        def read():
            for line in proc.stderr:
                events.put((time.perf_counter(), line))
            events.put((time.perf_counter(), None))

        Thread(target=read, daemon=True).start()

        def wait_for(marker):
            deadline = time.monotonic() + 30
            while True:
                try:
                    stamp, line = events.get(timeout=max(0, deadline - time.monotonic()))
                except Empty as error:
                    raise RuntimeError(f"timed out waiting for {marker}") from error
                if line is None:
                    raise RuntimeError(f"watch exited while waiting for {marker}")
                if marker in line:
                    return stamp

        errors = []
        fixes = []
        additions = []
        try:
            ready_ms=round(1000*(wait_for("jato watch: watching")-launched),1)
            wait_for("(exit 0)")
            status = Path(f"/proc/{proc.pid}/status")
            idle_kib = None
            if status.exists():
                idle_kib = int(next(l for l in status.read_text().splitlines() if l.startswith("VmRSS:")).split()[1])
            idle_pss = idle_tree_pss(proc.pid)
            for i in range(options.runs):
                start = time.perf_counter()
                source.write_text("pub fn parse() -> u8 { missing_value() }")
                errors.append(round(1000 * (wait_for("error[E0425]") - start), 1))
                wait_for("cargo check --workspace")
                wait_for("(exit 101)")
                start = time.perf_counter()
                source.write_text(good)
                wait_for("cargo check --workspace")
                fixes.append(round(1000 * (wait_for("(exit 0)") - start), 1))
                if options.directories:
                    staged=root/"incoming"
                    (staged/"nested").mkdir(parents=True)
                    (staged/"nested/input").write_text("before")
                    arrived=workspace/"parser"/f"batch-{i}"
                    start=time.perf_counter()
                    staged.rename(arrived)
                    wait_for("cargo check --workspace")
                    additions.append(round(1000*(wait_for("(exit 0)")-start),1))
                    (arrived/"nested/input").write_text("after")
                    wait_for("cargo check -p parser")
                    wait_for("cargo check --workspace")
                    wait_for("(exit 0)")
        finally:
            proc.send_signal(signal.SIGINT)
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
        print(json.dumps(dict(runs=options.runs, debounce_ms=options.debounce,
                              save_to_diagnostic_ms=errors, fix_to_workspace_green_ms=fixes,
                              median_diagnostic_ms=statistics.median(errors),
                              median_workspace_green_ms=statistics.median(fixes), idle_watch_rss_kib=idle_kib,
                              worktree=options.worktree, fast_profile=options.dev, idle_tree_pss_kib=idle_pss,
                              watched_leaf_directories=options.directories,start_to_watching_ms=ready_ms,
                              added_subtree_to_workspace_green_ms=additions,
                              median_subtree_ms=statistics.median(additions) if additions else None),
                         indent=2))


if __name__ == "__main__":
    main()
