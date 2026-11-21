#!/usr/bin/env python3
"""Measure wall time in disposable Cargo targets. Results go to stdout as JSON."""
import argparse
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project", type=Path, help="benchmark an existing locked Cargo project (clean, restored and fresh builds)")
    parser.add_argument("--jato", default="jato")
    parser.add_argument("--verify-bin", help="compare this release executable's --version output before and after restore")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--scratch", default=os.environ.get("TMPDIR") or "/var/tmp")
    options = parser.parse_args()
    if min(options.runs, options.jobs) < 1:
        parser.error("--runs and --jobs must be positive")
    jato = Path(shutil.which(options.jato) or options.jato).resolve(strict=True)
    cargo = os.environ.get("CARGO") or "cargo"
    with tempfile.TemporaryDirectory(dir=options.scratch, prefix="jato-bench-") as scratch:
        root = Path(scratch).resolve()
        project = options.project.resolve(strict=True) if options.project else root / "project"
        target = root / "target"
        cache = root / "cache"
        if options.project:
            if not (project / "Cargo.lock").is_file():
                parser.error("--project needs an existing Cargo.lock")
        else:
            shutil.copytree(Path(__file__).parent / "regex-app", project, ignore=shutil.ignore_patterns("target", "jato-bench-*"))
            # Cargo omits nested packages when creating a .crate archive.
            (project / "Cargo.toml.in").rename(project / "Cargo.toml")
        env = dict(os.environ, CARGO_BUILD_JOBS=str(options.jobs), RUSTC_WRAPPER="",
                   RUSTC_WORKSPACE_WRAPPER="", CARGO_TARGET_DIR=str(target),
                   CARGO_BUILD_BUILD_DIR=str(target), JATO_CACHE_DIR=str(cache),
                   JATO_MAX_SIZE="1GiB", CARGO=cargo)
        if options.offline:
            env["CARGO_NET_OFFLINE"] = "true"
        subprocess.run([cargo, "fetch", "--locked"], cwd=project, env=env, check=True)
        samples = {}
        peak_kib = {}
        # GNU time reports maximum RSS over the process tree (largest process).
        time_bin = Path("/usr/bin/time")
        gnu_time = time_bin.is_file() and "GNU" in subprocess.run(
            [time_bin, "--version"], capture_output=True, text=True).stdout.upper()

        def measure(label, tool, *args):
            cmd = [str(tool), *args, "--locked", "-q"]
            if gnu_time:
                cmd = [time_bin, "-f", "%M", "-o", root / "rss", *cmd]
            start = time.perf_counter()
            subprocess.run(cmd, cwd=project, env=env, check=True)
            samples.setdefault(label, []).append(round(time.perf_counter() - start, 4))
            if gnu_time:
                peak_kib.setdefault(label, []).append(int((root / "rss").read_text()))

        def clean():
            if target.exists():
                shutil.rmtree(target)

        def version_output():
            return subprocess.check_output([target / "release" / options.verify_bin, "--version"],
                                           cwd=project, env=env) if options.verify_bin else None

        for _ in range(options.runs):
            clean()
            measure("cargo_clean_release", cargo, "build", "--release")
            measure("cargo_fresh_release", cargo, "build", "--release")
            expected_version = version_output()
            clean()
            if cache.exists():
                shutil.rmtree(cache)
            measure("jato_cold_release", jato, "build", "--release")
            clean()
            measure("jato_restored_release", jato, "build", "--release")
            measure("jato_fresh_release", jato, "build", "--release")
            if version_output() != expected_version:
                raise RuntimeError("restored executable produced different version output")
            if not options.project:
                result = subprocess.check_output([target / "release/jato-regex-bench"], text=True)
                assert result == "3\n", result
            for name, tool in [("cargo", cargo), ("jato", jato)]:
                clean()
                measure(name + "_clean_check", tool, "check")
                if not options.project:
                    source = project / "src/main.rs"
                    original = source.read_text()
                    source.write_text(original.replace("one two three", "one two"))
                    measure(name + "_edited_check", tool, "check")
                    source.write_text(original)
                    measure(name + "_undo_check", tool, "check")
                clean()
                measure(name + "_repeated_clean_check", tool, "check")
        stats = json.loads(subprocess.check_output([jato, "stats", "--json"], env=env))
        print(json.dumps(dict(rustc=subprocess.check_output(["rustc", "-V"], text=True).strip(),
                              cargo=subprocess.check_output([cargo, "-V"], text=True).strip(),
                              runs=options.runs, jobs=options.jobs, samples_seconds=samples,
                              median_seconds={k: statistics.median(v) for k, v in samples.items()},
                              peak_rss_kib=peak_kib, cache=stats), indent=2))


if __name__ == "__main__":
    main()
