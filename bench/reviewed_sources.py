#!/usr/bin/env python3
"""Build reviewed registry sources from their archives, then change the macro, cfg and a provider."""
import argparse
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import tomllib


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jato", default="jato")
    parser.add_argument("--scratch", default=os.environ.get("TMPDIR", "/var/tmp"))
    opts = parser.parse_args()
    jato = Path(shutil.which(opts.jato) or opts.jato).resolve(strict=True)
    rustc = Path(subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip()) / "bin/rustc"
    cargo = rustc.with_name("cargo")
    registry = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo")) / "registry"
    with tempfile.TemporaryDirectory(prefix="review-", dir=opts.scratch) as temp:
        root = Path(temp)
        project = root / "project"
        project.mkdir()
        (project / "src").mkdir()
        (project / "src/main.rs").write_text(
            'fn main(){ let _:libc::c_int=0; let _:Option<cmov::Condition>=None;'
            'let v=num_bigint::BigUint::from(7u8); assert_eq!(v,num_traits::FromPrimitive::from_u8(7).unwrap());'
            'println!("ok"); }')
        (project / "Cargo.toml").write_text(
            '[package]\nname="review_consumer"\nversion="0.0.0"\nedition="2021"\n'
            '[dependencies]\nlibc={version="=0.2.191",features=["extra_traits"]}\ncmov="=0.5.4"\n'
            'num-bigint="=0.4.8"\nnum-traits={version="=0.2.19",features=["libm","i128"]}\n'
            'num-integer={version="=0.1.47",features=["i128","std"]}\n'
            '[profile.dev]\nopt-level=3\ndebug-assertions=true\nincremental=false\n')
        home = root / "cargo"
        home.mkdir()
        (home / "registry").symlink_to(registry.resolve())
        target = root / "target"
        env = dict(os.environ, RUSTC=str(rustc), RUSTC_WRAPPER=str(jato), CARGO_HOME=str(home),
                   CARGO_TARGET_DIR=str(target), CARGO_BUILD_BUILD_DIR=str(target),
                   JATO_CACHE_DIR=str(root / "cache"), JATO_LOG="debug", CARGO_NET_OFFLINE="true")
        for key in ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WORKSPACE_WRAPPER", "JATO_DISABLE"]:
            env.pop(key, None)
        # Patched path dependencies are local Cargo packages; preserve the cap
        # normally used for the registry copies on this nightly.
        env["RUSTFLAGS"] = "--cap-lints allow"
        def run(args, **extra):
            result = subprocess.run(args, cwd=project, env={**env, **extra}, capture_output=True, text=True)
            if result.returncode:
                raise RuntimeError(result.stderr[-4000:])
            return result
        def extract(name):
            package = next(p for p in tomllib.loads((project/"Cargo.lock").read_text())["package"] if p["name"] == name)
            stem = f"{name}-{package['version']}"
            archive = next((registry/"cache").glob(f"*/{stem}.crate"))
            with archive.open("rb") as stream:
                assert hashlib.file_digest(stream, "sha256").hexdigest() == package["checksum"]
            with tarfile.open(archive) as stream:
                stream.extractall(root, filter="data")
            return root/stem
        def build(**extra):
            if target.is_dir():
                shutil.rmtree(target)
            return run([cargo, "build", "--locked", "--offline"], **extra).stderr
        run([cargo, "generate-lockfile", "--offline"])
        libc = extract("libc")
        traits = extract("num-traits")
        with (project/"Cargo.toml").open("a") as f:
            f.write(f'[patch.crates-io]\nlibc={{path="{libc}"}}\nnum-traits={{path="{traits}"}}\n')
        run([cargo, "generate-lockfile", "--offline"])
        build()
        warm = build()
        for name in ["libc", "cmov", "num_bigint"]:
            assert f"jato [{name}] hit" in warm, warm[-5000:]
        assert run([target/"debug/review_consumer"]).stdout == "ok\n"
        original = (libc/"src/macros.rs").read_bytes()
        (libc/"src/macros.rs").write_bytes(original + b"\n// changed macro provider\n")
        assert "jato [libc] compile succeeded, skipping store: assembly input outside the reviewed source set" in build()
        (libc/"src/macros.rs").write_bytes(original)
        assert "jato [libc] hit" in build()
        assert "assembly input outside the reviewed source set" in build(RUSTFLAGS="--cap-lints allow --cfg another_build")
        with (traits/"src/lib.rs").open("a") as f:
            f.write("\n#[doc(hidden)] pub const CHANGED_PROVIDER: usize = 23;\n")
        changed = build()
        assert "jato [num_bigint] compile succeeded, skipping store: assembly input has an unverified source provider" in changed, changed[-5000:]
        print("Verified clean restores, edited source, cfg selection, changed provider, and executable output.")


if __name__ == "__main__":
    main()
