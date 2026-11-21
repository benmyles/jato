#!/usr/bin/env python3
"""Measure concurrent Cargo builds sharing intermediates and separate delivery targets."""
import argparse
from contextlib import ExitStack
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

from compare import binary, run


SCRIPT = """
use std::{env, fs, thread, time::{Duration, SystemTime, UNIX_EPOCH}};
fn now() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
}
fn main() {
    let start = now();
    thread::sleep(Duration::from_millis(800));
    fs::write(env::var("JATO_TRIAL_LOG").unwrap(), format!("{} {}", start, now())).unwrap();
    println!("cargo:rerun-if-env-changed=JATO_TRIAL_LOG");
}
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jato", default="jato")
    parser.add_argument("--runs", type=int, default=2)
    parser.add_argument("--scratch", default=os.environ.get("TMPDIR") or "/var/tmp")
    opts = parser.parse_args()
    if opts.runs < 1:
        parser.error("--runs must be positive")
    jato = binary(opts.jato)
    cargo = os.environ.get("CARGO", "cargo")
    with tempfile.TemporaryDirectory(dir=opts.scratch, prefix="js-") as temp:
        root = Path(temp).resolve()
        project = root / "workspace"
        project.mkdir()
        names = ["one", "two"]
        (project / "Cargo.toml").write_text('[workspace]\nresolver="2"\nmembers=["one","two"]\n')
        for name in names:
            package = project / name
            (package / "src").mkdir(parents=True)
            (package / "Cargo.toml").write_text(f'[package]\nname="{name}"\nversion="0.1.0"\nedition="2021"\n')
            (package / "src/main.rs").write_text(f'fn main() {{ println!("{name}"); }}')
            (package / "build.rs").write_text(SCRIPT)
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(("JATO_", "CARGO_BUILD_", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"))}
        env.update(CARGO=cargo, CARGO_NET_OFFLINE="true", CARGO_HOME=str(root / "cargo"), CARGO_BUILD_JOBS="2")
        version = run([cargo, "-V"], env, project).decode().strip()
        if "nightly" not in version or b"fine-grain-locking" not in run([cargo, "-Zhelp"], env, project):
            parser.error("select a nightly Cargo with -Zfine-grain-locking")
        run([cargo, "generate-lockfile", "--offline"], env, project)
        rows = {"standard": [], "fine-grain": []}
        for trial in range(opts.runs):
            for mode in (rows if trial % 2 == 0 else reversed(rows)):
                work = root / f"{mode}-{trial}"
                work.mkdir()
                jobs = []
                windows = []
                start = time.perf_counter()
                with ExitStack() as children:
                    for name in names:
                        target = work / name
                        log = work / (name + ".log")
                        cfg = dict(env, CARGO_HOME=str(work / "cargo"), CARGO_TARGET_DIR=str(target),
                                   JATO_CACHE_DIR=str(work / "cache"), JATO_TRIAL_LOG=str(log))
                        cmd = [jato, "--keep-build", "cargo"]
                        if mode == "fine-grain":
                            cmd.append("-Zfine-grain-locking")
                        cmd.extend(["build", "--locked", "-p", name])
                        child = children.enter_context(subprocess.Popen(cmd, cwd=project, env=cfg))
                        jobs.append((name, child, cfg, target, log))
                    for _, child, _, _, _ in jobs:
                        if code := child.wait():
                            raise subprocess.CalledProcessError(code, child.args)
                elapsed = time.perf_counter() - start
                for name, _, cfg, target, log in jobs:
                    assert run([target / "debug" / name], cfg, project).decode().strip() == name
                    windows.append([int(t) for t in log.read_text().split()])
                overlap = max(0, min(w[1] for w in windows) - max(w[0] for w in windows))
                rows[mode].append(dict(seconds=round(elapsed, 4), script_overlap_ms=overlap))
        print(json.dumps(dict(cargo=version,
                              jato=run([jato, "-V"], env, project).decode().strip(),
                              script_sleep_ms=800, samples=rows), indent=2))


if __name__ == "__main__":
    main()
