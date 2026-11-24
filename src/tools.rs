//! Compilers other than rustc that a build starts: the C and C++ compilers
//! and the assemblers its build scripts run.
//!
//! Cargo gives Jato no say in what a build script starts. For the builds
//! Jato runs it therefore puts a directory at the head of PATH in which
//! the usual names of those programs lead back to Jato, which finds the
//! program itself further along PATH and asks the build's supervisor for
//! its result, as it does for rustc.
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::{env, fs};

/// The names Jato answers to.
pub const NAMES: &[&str] = &[
    "cc", "c++", "gcc", "g++", "clang", "clang++", "nasm", "yasm",
];

/// Names the directory of stand-ins to everything a build starts, so that
/// Jato can leave it out when it looks for the programs themselves.
pub const DIRECTORY: &str = "JATO_STAND_INS";

/// Have `cmd`, a Cargo command, find Jato under compilers' names.
pub fn offer(cmd: &mut std::process::Command, cache: &Path) {
    let path = env::var_os("PATH").unwrap_or_default();
    if !wanted() {
        return;
    }
    let Some(stand_ins) = directory(cache, &path) else {
        return;
    };
    let dirs = std::iter::once(stand_ins.clone()).chain(env::split_paths(&path));
    if let Ok(with) = env::join_paths(dirs) {
        cmd.env("PATH", with).env(DIRECTORY, stand_ins);
    }
}

/// Whether the builds Jato runs are given the directory.
pub fn wanted() -> bool {
    cfg!(target_os = "linux") && env::var_os("JATO_TOOLS").is_none_or(|v| v != "0")
}

/// The name this process was started under, if it is one of `NAMES`.
pub fn invoked_as(program: &OsStr) -> Option<&'static str> {
    let name = Path::new(program).file_name()?.to_str()?;
    NAMES.iter().copied().find(|known| *known == name)
}

/// Whether a program of this name is a C or C++ compiler of the two
/// families that take the same options: `cc`, `g++-14`, `clang`,
/// `x86_64-linux-gnu-gcc`.
pub fn c_family(name: &str) -> bool {
    let unversioned = name
        .rsplit_once('-')
        .filter(|(_, version)| {
            !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        })
        .map_or(name, |(rest, _)| rest);
    let plain = unversioned.rsplit('-').next().unwrap_or(unversioned);
    matches!(plain, "cc" | "c++" | "gcc" | "g++" | "clang" | "clang++")
}

/// `path` without the directory `dir`.
pub fn without(path: &OsStr, dir: &OsStr) -> OsString {
    let kept = env::split_paths(path).filter(|entry| entry.as_os_str() != dir);
    env::join_paths(kept).unwrap_or_else(|_| path.to_owned())
}

/// The entries of `path` that follow `dir`, if `dir` appears in `path`.
/// Used when a stand-in is invoked directly by a wrapper placed ahead of it
/// on `PATH`, so looking up the real compiler does not loop back to the wrapper.
pub fn after(path: &OsStr, dir: &OsStr) -> Option<OsString> {
    let mut dirs = env::split_paths(path);
    dirs.find(|entry| entry.as_os_str() == dir)?;
    let rest: Vec<_> = dirs.filter(|entry| entry.as_os_str() != dir).collect();
    env::join_paths(rest).ok()
}

/// Where `path` finds the program `name`: a file that may be run and is
/// not this program under another name.
pub fn find(name: &str, path: &OsStr) -> Option<PathBuf> {
    let own = env::current_exe().and_then(fs::canonicalize).ok();
    env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| {
            fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
                && fs::canonicalize(candidate).ok() != own
        })
}

/// A directory in which each of `NAMES` that `path` finds leads to this
/// program, made inside the cache if it is not there yet. None if none of
/// them is found or the directory cannot be made.
///
/// A name `path` does not find is left out: a build script that asks
/// whether a compiler is installed must get the answer it would have had.
pub fn directory(cache: &Path, path: &OsStr) -> Option<PathBuf> {
    let own = env::current_exe().ok()?;
    let found: Vec<&str> = NAMES
        .iter()
        .copied()
        .filter(|name| find(name, path).is_some())
        .collect();
    if found.is_empty() {
        return None;
    }
    let mut hash = blake3::Hasher::new_derive_key("jato stand-ins v1");
    hash.update(own.as_os_str().as_bytes());
    for name in &found {
        hash.update(&[0]);
        hash.update(name.as_bytes());
    }
    let all = cache.join("stand-ins");
    let dir = all.join(&hash.finalize().to_hex().as_str()[..16]);
    let complete = |dir: &Path| {
        found
            .iter()
            .all(|name| fs::read_link(dir.join(name)).is_ok_and(|to| to == own))
    };
    if complete(&dir) {
        return Some(dir);
    }
    // Made under another name and moved into place whole, so that a build
    // started meanwhile never finds half of it.
    let making = all.join(format!("making-{}", std::process::id()));
    let _ = fs::remove_dir_all(&making);
    fs::create_dir_all(&making).ok()?;
    for name in &found {
        std::os::unix::fs::symlink(&own, making.join(name)).ok()?;
    }
    if fs::rename(&making, &dir).is_err() {
        let _ = fs::remove_dir_all(&making);
    }
    complete(&dir).then_some(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_compilers_are_known_by_their_names() {
        for name in [
            "cc",
            "c++",
            "gcc",
            "g++-14",
            "clang",
            "clang++-23",
            "x86_64-linux-gnu-gcc",
            "aarch64-linux-gnu-g++-13.2",
        ] {
            assert!(c_family(name), "{name}");
        }
        for name in [
            "nasm",
            "yasm",
            "rustc",
            "ld",
            "gcc-ar",
            "cc1",
            "clang-format",
        ] {
            assert!(!c_family(name), "{name}");
        }
        assert_eq!(invoked_as(OsStr::new("/a/stand-ins/c++")), Some("c++"));
        assert_eq!(invoked_as(OsStr::new("jato")), None);
    }

    #[test]
    fn the_directory_of_stand_ins_is_left_out_of_a_search() {
        let path = OsStr::new("/a/stand-ins:/usr/bin:/a/stand-ins:/bin");
        assert_eq!(without(path, OsStr::new("/a/stand-ins")), "/usr/bin:/bin");
        assert_eq!(
            after(
                OsStr::new("/shim:/a/stand-ins:/usr/bin:/bin"),
                OsStr::new("/a/stand-ins")
            ),
            Some(OsString::from("/usr/bin:/bin"))
        );
        assert_eq!(after(path, OsStr::new("/missing")), None);
    }
}
