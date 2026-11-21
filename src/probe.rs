//! Cargo asks rustc for version and target facts before scheduling any crate.
//! These queries have no output files. Restrict stdin-bearing invocations to
//! Cargo's empty target-info probe, and retain the exact two output streams.
use crate::{compiler, fingerprint, store::Store};
use anyhow::Result;
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

fn eligible(args: &[OsString]) -> bool {
    if args == ["-vV"] || args == ["--version"] {
        return true;
    }
    if args.first().is_none_or(|v| v != "-")
        || args.len() > 96
        || std::fs::read_link("/proc/self/fd/0").ok().as_deref() != Some(Path::new("/dev/null"))
    {
        return false;
    }
    let mut it = args[1..].iter().map(|v| v.to_str());
    let mut named = false;
    let mut prints = false;
    while let Some(option) = it.next() {
        match option {
            Some("--crate-name") if !named && it.next() == Some(Some("___")) => named = true,
            Some("--crate-type")
                if it.next().flatten().is_some_and(|v| {
                    matches!(
                        v,
                        "bin" | "lib" | "rlib" | "cdylib" | "dylib" | "staticlib" | "proc-macro"
                    )
                }) => {}
            Some("--target")
                if it.next().flatten().is_some_and(|v| {
                    !v.is_empty()
                        && v.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                }) => {}
            Some("--cfg") if it.next().flatten().is_some() => (),
            Some("-Z")
                if it
                    .next()
                    .flatten()
                    .is_some_and(crate::invocation::supported_unstable) => {}
            Some(arg)
                if arg
                    .strip_prefix("-Z")
                    .is_some_and(crate::invocation::supported_unstable) => {}
            Some("-Wwarnings") => (),
            Some("-C")
                if it.next().flatten().is_some_and(|v| {
                    v.starts_with("linker=") || v.starts_with("target-feature=")
                }) => {}
            Some(s)
                if matches!(
                    s.strip_prefix("--print="),
                    Some(
                        "file-names"
                            | "sysroot"
                            | "split-debuginfo"
                            | "crate-name"
                            | "cfg"
                            | "target-libdir"
                    )
                ) =>
            {
                prints = true
            }
            _ => return false,
        }
    }
    named && prints
}

pub fn run(compiler_path: &OsStr, args: &[OsString]) -> Result<Option<i32>> {
    if Path::new(compiler_path).file_name() != Some(OsStr::new("rustc")) || !eligible(args) {
        return Ok(None);
    }
    let Ok(mut store) = Store::open() else {
        return Ok(None);
    };
    let Ok(recipe) = fingerprint::recipe(compiler_path, args, false, &mut store) else {
        return Ok(None);
    };
    if let Ok(Some((stdout, stderr))) = store.probe(&recipe.key) {
        io::stdout().write_all(&stdout)?;
        io::stderr().write_all(&stderr)?;
        let _ = store.count("probe_hit", 1);
        return Ok(Some(0));
    }
    let out = Command::new(compiler_path).args(args).output()?;
    io::stdout().write_all(&out.stdout)?;
    io::stderr().write_all(&out.stderr)?;
    if out.status.success() {
        let _ = store.save_probe(&recipe.key, &out.stdout, &out.stderr);
        if args == ["-vV"]
            && let Ok(text) = std::str::from_utf8(&out.stdout)
            && let Some(release) = text.lines().find_map(|l| l.strip_prefix("release: "))
        {
            let _ = store.record_compiler(
                &recipe.toolchain,
                crate::admission::Channel::release(release),
            );
        }
    }
    Ok(Some(compiler::exit_code(out.status)))
}
