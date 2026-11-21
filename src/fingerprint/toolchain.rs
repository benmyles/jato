//! Resolve the compiler Cargo actually selected and identify its installed libraries.
use super::{field, stamp};
use crate::store::{Digest, Store};
use anyhow::{Context, Result, ensure};
use std::env;
use std::ffi::{CString, OsStr};
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;

fn stamp_path(hash: &mut blake3::Hasher, path: &Path) -> Result<()> {
    field(hash, path.as_os_str().as_bytes());
    for n in stamp(&fs::metadata(path)?) {
        hash.update(&n.to_le_bytes());
    }
    Ok(())
}

fn ordered(dir: &Path, include: Option<fn(&fs::FileType) -> bool>) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if let Some(include) = include {
            let ty = entry.file_type()?;
            let keep = if ty.is_symlink() {
                fs::metadata(entry.path()).is_ok_and(|m| include(&m.file_type()))
            } else {
                include(&ty)
            };
            if !keep {
                continue;
            }
        }
        paths.push(entry.path());
    }
    paths.sort();
    Ok(paths)
}

fn executable_path(bin: &Path, cwd: &Path) -> Result<PathBuf> {
    if bin.as_os_str().as_bytes().contains(&b'/') {
        return Ok(cwd.join(bin));
    }
    env::split_paths(&env::var_os("PATH").context("compiler PATH is unset")?)
        .map(|dir| cwd.join(dir).join(bin))
        .find(|path| {
            path.is_file()
                && CString::new(path.as_os_str().as_bytes()).is_ok_and(|name| unsafe {
                    libc::faccessat(libc::AT_FDCWD, name.as_ptr(), libc::X_OK, libc::AT_EACCESS)
                        == 0
                })
        })
        .context("compiler missing from PATH")
}

// Only infer a PATH compiler's sysroot for a physical rustup distribution.
// Linked toolchains, distro launchers and other layouts can choose a different
// sysroot at runtime. For those, ask the original compiler.
fn distribution_root(path: &Path) -> Option<PathBuf> {
    let root = path.parent()?.parent()?;
    if !cfg!(target_os = "linux")
        || !path.ends_with("bin/rustc")
        || !root.parent()?.ends_with("toolchains")
        || fs::canonicalize(path).ok()? != path
        || !root
            .join("lib/rustlib/multirust-channel-manifest.toml")
            .is_file()
    {
        return None;
    }
    let lib = root.join("lib");
    if fs::canonicalize(&lib).ok()? != lib {
        return None;
    }
    // Reject ELF launchers which do not refer to this installed rustc_driver.
    // A match only proposes a root for the confirmation below.
    let mut header = [0; 8192];
    File::open(path).ok()?.read_exact(&mut header).ok()?;
    let start = memchr::memmem::find(&header, b"librustc_driver-")?;
    let end = memchr::memchr(0, &header[start..])?;
    let name = &header[start..start + end];
    (header.starts_with(b"\x7fELF")
        && name.ends_with(b".so")
        && !name.contains(&b'/')
        && fs::symlink_metadata(lib.join(OsStr::from_bytes(name)))
            .ok()?
            .is_file())
    .then(|| root.to_owned())
}

fn reported_path(cmd: &mut Command) -> Result<PathBuf> {
    let out = cmd.output()?;
    ensure!(out.status.success(), "cannot resolve compiler toolchain");
    let bytes = out.stdout.strip_suffix(b"\n").unwrap_or(&out.stdout);
    let path = Path::new(OsStr::from_bytes(bytes));
    ensure!(path.is_absolute(), "toolchain reported a relative path");
    Ok(path.to_owned())
}

fn identity(root: &Path) -> Result<Digest> {
    let mut identity = blake3::Hasher::new_derive_key("jato local toolchain v1");
    stamp_path(&mut identity, &root.join("bin/rustc"))?;
    for p in ordered(&root.join("lib"), Some(fs::FileType::is_file))? {
        stamp_path(&mut identity, &p)?;
    }
    for target in ordered(&root.join("lib/rustlib"), Some(fs::FileType::is_dir))? {
        let dir = target.join("lib");
        if dir.is_dir() {
            for p in ordered(&dir, None)? {
                stamp_path(&mut identity, &p)?;
            }
        }
    }
    Ok(*identity.finalize().as_bytes())
}

fn rustup_proxy(executable: &Path) -> Option<PathBuf> {
    let proxy = executable.with_file_name("rustup");
    let dispatcher = fs::metadata(&proxy).ok()?;
    let selected = fs::metadata(executable).ok()?;
    (dispatcher.dev() == selected.dev() && dispatcher.ino() == selected.ino()).then_some(proxy)
}

pub(super) fn resolve(
    compiler: &OsStr,
    cwd: &Path,
    store: &mut Store,
) -> Result<(PathBuf, Digest)> {
    let bin = Path::new(compiler);
    let executable = executable_path(bin, cwd)?;
    if let Some(root) = bin.parent().and_then(Path::parent)
        && root.is_absolute()
        && executable == root.join("bin/rustc")
        && root.join("lib/rustlib").is_dir()
    {
        return Ok((root.to_owned(), identity(root)?));
    }

    let proxy = rustup_proxy(&executable);
    // A custom loader path or a symlink spelled in rustc's argv[0] can select
    // a different sysroot. Let rustc inspect either layout itself.
    let candidate = if env::var_os("LD_LIBRARY_PATH").is_some_and(|v| !v.is_empty()) {
        None
    } else if let Some(proxy) = &proxy {
        // Rustup applies the project's toolchain file and all overrides.
        reported_path(Command::new(proxy).args(["which", "rustc"]))
            .ok()
            .and_then(|path| distribution_root(&path))
    } else if bin.is_symlink() {
        None
    } else {
        distribution_root(&executable)
    };
    let mut observed = None;
    if let Some(candidate) = candidate
        && let Ok(id) = identity(&candidate)
    {
        let mut key = blake3::Hasher::new_derive_key("jato observed distribution sysroot v1");
        key.update(&id);
        field(&mut key, compiler.as_bytes());
        stamp_path(&mut key, &executable)?;
        let key = *key.finalize().as_bytes();
        let expected = candidate.as_os_str().as_bytes();
        if store
            .probe(&key)?
            .is_some_and(|(value, err)| value == expected && err.is_empty())
        {
            return Ok((candidate, id));
        }
        let actual = reported_path(Command::new(compiler).args(["--print", "sysroot"]))?;
        if actual == candidate {
            let _ = store.save_probe(&key, expected, &[]);
            return Ok((candidate, id));
        }
        observed = Some(actual);
    }
    let root = match observed {
        Some(path) => path,
        None => reported_path(Command::new(compiler).args(["--print", "sysroot"]))?,
    };
    let bundled = root.join("bin/rustc");
    ensure!(
        proxy.is_some() || fs::canonicalize(&executable)? == fs::canonicalize(bundled)?,
        "custom rustc driver"
    );
    let id = identity(&root)?;
    Ok((root, id))
}
