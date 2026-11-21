#!/usr/bin/env python3
"""Reproduce an assembly source-set receipt from Cargo dep-info and an authenticated .crate archive."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tomllib


def settings(args):
    options = []
    externs = 0
    it = iter(args)
    for word in it:
        if word.startswith("--"):
            key, eq, val = word.partition("=")
            if key in ("--cfg", "--edition", "--extern", "--target"):
                value = val if eq else next(it)
                if key == "--extern":
                    key = f"--extern[{externs}]"
                    if "=" in value:
                        externs += 1
                    value = value.split("=", 1)[0]
                options.append(f"{key}={value}")
        elif word.startswith("-C"):
            option = word[2:] or next(it)
            if option.split("=", 1)[0] in ("target-feature", "target-cpu", "panic", "debug-assertions"):
                options.append("-C" + option)
    return sorted(options)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("archive", "lock", "dep-info", "rustc-args", "crate-root"):
        parser.add_argument("--" + name, required=True, type=Path)
    parser.add_argument("--b3sum", default="b3sum", help="standard BLAKE3 command")
    opts = parser.parse_args()
    packages = tomllib.loads(opts.lock.read_text())["package"]
    stem = opts.archive.name.removesuffix(".crate")
    original = next(p for p in packages if f"{p['name']}-{p['version']}" == stem and "checksum" in p)
    with opts.archive.open("rb") as f:
        if hashlib.file_digest(f, "sha256").hexdigest() != original["checksum"]:
            parser.error("archive differs from the Cargo.lock checksum")
    root = opts.crate_root.absolute()
    if root.parts[-2:] != ("src", "lib.rs"):
        parser.error("--crate-root must be the compiler's src/lib.rs")
    base = root.parent.parent
    lines = opts.dep_info.read_text().splitlines()
    files = sorted({Path(l[:-1]).absolute() for l in lines if l.endswith(":") and not l.startswith("#")})
    tracked = [l.removeprefix("# env-dep:") for l in lines if l.startswith("# env-dep:")]
    generated = None
    if stem == "serde_core-1.0.229" and len(tracked) == 1 and tracked[0].startswith("OUT_DIR="):
        generated = Path(tracked[0].split("=", 1)[1]) / "private.rs"
    elif not files or tracked:
        parser.error("unexpected source closure or tracked environment")
    def digest(data):
        return bytes.fromhex(subprocess.check_output([opts.b3sum, "--no-names"], input=data).decode().strip())
    def field(data):
        return len(data).to_bytes(8, "little") + data
    signature = bytearray(field(b"jato reviewed Rust assembly inputs v1"))
    options = settings(json.loads(opts.rustc_args.read_text()))
    for option in options:
        signature.extend(field(option.encode()))
    signature.extend(field(b"files"))
    records = []
    with tarfile.open(opts.archive) as archive:
        for path in files:
            if path == generated:
                data = path.read_bytes()
                if data != b"#[doc(hidden)]\npub mod __private229 {\n    #[doc(hidden)]\n    pub use crate::private::*;\n}\n":
                    parser.error("unreviewed generated serde_core private module")
                records.append(digest(field(b"OUT_DIR/private.rs") + field(digest(data))))
                continue
            logical = path.relative_to(base)
            physical = path.resolve(strict=True).relative_to(base.resolve(strict=True))
            member = archive.getmember(f"{stem}/{physical.as_posix()}")
            if not member.isfile() or member.size > 8 << 20:
                parser.error(f"unexpected archive member {member.name}")
            data = archive.extractfile(member).read()
            if data != path.read_bytes():
                parser.error(f"local source differs from archive member {member.name}")
            parts = (b"package", os.fsencode(logical), os.fsencode(physical), digest(data))
            records.append(digest(b"".join(field(v) for v in parts)))
    for record in sorted(records):
        signature.extend(field(record))
    print(json.dumps(dict(package=stem, archive_sha256=original["checksum"], files=len(files),
                          settings=options, receipt=digest(signature).hex()), indent=2))


if __name__ == "__main__":
    main()
