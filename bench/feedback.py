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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jato", default="jato")
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--debounce", type=int, help="override the installed watcher’s default")
    parser.add_argument("--scratch", default=os.environ.get("TMPDIR") or "/var/tmp")
    options = parser.parse_args()
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
        source = workspace / "parser/src/lib.rs"
        good = "pub fn parse() -> u8 { 42 }"
        source.write_text(good)
        (workspace / "client/src/lib.rs").write_text("pub fn run() -> u8 { parser::parse() }")
        env = dict(os.environ, JATO_CACHE_DIR=str(root / "cache"), CARGO_HOME=str(root / "cargo"),
                   CARGO_TARGET_DIR=str(root / "target"), CARGO_BUILD_BUILD_DIR=str(root / "target"),
                   CARGO_NET_OFFLINE="true", RUSTC_WRAPPER="", RUSTC_WORKSPACE_WRAPPER="")
        subprocess.run([jato, "check", "--workspace", "-q"], cwd=workspace, env=env, check=True)
        watch_args = [] if options.debounce is None else ["--debounce", str(options.debounce)]
        proc = subprocess.Popen([jato, "watch", *watch_args], cwd=workspace,
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
        try:
            wait_for("(exit 0)")
            status = Path(f"/proc/{proc.pid}/status")
            idle_kib = None
            if status.exists():
                idle_kib = int(next(l for l in status.read_text().splitlines() if l.startswith("VmRSS:")).split()[1])
            for _ in range(options.runs):
                start = time.perf_counter()
                source.write_text("pub fn parse() -> u8 { missing_value() }")
                errors.append(round(1000 * (wait_for("error[E0425]") - start), 1))
                wait_for("cargo check --workspace")
                wait_for("(exit 101)")
                start = time.perf_counter()
                source.write_text(good)
                wait_for("cargo check --workspace")
                fixes.append(round(1000 * (wait_for("(exit 0)") - start), 1))
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
                              median_workspace_green_ms=statistics.median(fixes), idle_watch_rss_kib=idle_kib),
                         indent=2))


if __name__ == "__main__":
    main()
