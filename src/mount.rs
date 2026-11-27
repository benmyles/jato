//! Optional unprivileged Linux mount namespace for canonical target paths.
//!
//! `rustc` hashes the canonical path of `--out-dir` and every `--extern` `.rlib`
//! into its incremental dependency graph (`used_crate_source` / `CompileCodegenUnit`).
//! Across different git worktrees or scratch `CARGO_TARGET_DIR`s, differing real
//! paths invalidate codegen units for binaries and prevent handing saved state to
//! out-of-tree binary builds.
//!
//! When unprivileged user+mount namespaces are permitted by the kernel (and
//! `JATO_MOUNT != 0`), Jato bind-mounts the build's real target directory onto a
//! stable canonical mountpoint inside the cache directory before executing `rustc`,
//! rewriting target-directory compiler flags (`--out-dir`, `--extern`, `-L`) to
//! that canonical path while keeping `WORK` and `CARGO_MANIFEST_DIR` real.
//! Systems and containers that restrict unprivileged user namespaces fall back
//! automatically to the existing `/proc/self/cwd` and descriptor-alias paths.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::fs;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    use std::sync::{Mutex, OnceLock};

    /// Pre-allocated C strings and ID maps so `enter()` is async-signal-safe
    /// inside `CommandExt::pre_exec` (zero heap allocations after `fork`).
    pub struct Plan {
        real_target: CString,
        mountpoint: CString,
        root: CString,
        setgroups: CString,
        uid_map_path: CString,
        gid_map_path: CString,
        uid_map: [u8; 64],
        uid_len: usize,
        gid_map: [u8; 64],
        gid_len: usize,
    }

    fn format_id_map(id: u32) -> ([u8; 64], usize) {
        let s = format!("{id} {id} 1\n");
        let bytes = s.as_bytes();
        let mut buf = [0u8; 64];
        let len = bytes.len().min(buf.len());
        buf[..len].copy_from_slice(&bytes[..len]);
        (buf, len)
    }

    unsafe fn write_proc_file(path: *const libc::c_char, data: &[u8]) -> io::Result<()> {
        let fd = unsafe { libc::open(path, libc::O_WRONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        let err = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        if n != data.len() as isize {
            return Err(err);
        }
        Ok(())
    }

    impl Plan {
        pub fn new(real_target: &Path, mountpoint: &Path) -> io::Result<Self> {
            fs::create_dir_all(real_target)?;
            if let Some(parent) = mountpoint.parent() {
                fs::create_dir_all(parent)?;
            }
            if !mountpoint.exists() {
                let mut b = fs::DirBuilder::new();
                b.recursive(true).mode(0o700);
                let _ = b.create(mountpoint);
            }
            let uid = unsafe { libc::geteuid() };
            let gid = unsafe { libc::getegid() };
            let (uid_map, uid_len) = format_id_map(uid);
            let (gid_map, gid_len) = format_id_map(gid);
            let cstr = |p: &Path| {
                CString::new(p.as_os_str().as_bytes())
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))
            };
            Ok(Self {
                real_target: cstr(real_target)?,
                mountpoint: cstr(mountpoint)?,
                root: CString::new("/").unwrap(),
                setgroups: CString::new("/proc/self/setgroups").unwrap(),
                uid_map_path: CString::new("/proc/self/uid_map").unwrap(),
                gid_map_path: CString::new("/proc/self/gid_map").unwrap(),
                uid_map,
                uid_len,
                gid_map,
                gid_len,
            })
        }

        /// Finish configuring user/mount mappings and the target bind-mount
        /// immediately after `unshare(CLONE_NEWUSER | CLONE_NEWNS)`.
        /// Async-signal-safe.
        unsafe fn finish_after_unshare(&self) -> io::Result<()> {
            // Writing "deny" to setgroups is required before unprivileged gid_map.
            let _ = unsafe { write_proc_file(self.setgroups.as_ptr(), b"deny\n") };
            unsafe {
                write_proc_file(self.uid_map_path.as_ptr(), &self.uid_map[..self.uid_len])?;
                write_proc_file(self.gid_map_path.as_ptr(), &self.gid_map[..self.gid_len])?;
            }
            if unsafe {
                libc::mount(
                    std::ptr::null(),
                    self.root.as_ptr(),
                    std::ptr::null(),
                    libc::MS_REC | libc::MS_PRIVATE,
                    std::ptr::null(),
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            if unsafe {
                libc::mount(
                    self.real_target.as_ptr(),
                    self.mountpoint.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND | libc::MS_REC,
                    std::ptr::null(),
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// Enter a private unprivileged user+mount namespace and bind-mount
        /// `real_target` onto `mountpoint`. Async-signal-safe.
        pub unsafe fn enter(&self) -> io::Result<()> {
            if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) } != 0 {
                return Err(io::Error::last_os_error());
            }
            unsafe { self.finish_after_unshare() }
        }
    }

    pub fn attach(cmd: &mut Command, real_target: &Path, mountpoint: &Path) -> io::Result<()> {
        let plan = Plan::new(real_target, mountpoint)?;
        unsafe {
            cmd.pre_exec(move || plan.enter());
        }
        Ok(())
    }

    /// Enter a private unprivileged user+mount namespace in the current process
    /// right before `exec()` (when no `fork()` precedes `unshare()`).
    ///
    /// On Linux, `pthread_join()` (used by `std::thread::scope` in
    /// `give_saved_state` / `place_saved`) wakes on `CLONE_CHILD_CLEARTID`
    /// microseconds before the exiting thread finishes kernel `do_exit()`.
    /// Calling `unshare(CLONE_NEWUSER)` in that window fails with `EINVAL`
    /// without altering process state; retrying briefly closes the race.
    pub fn enter_for_exec(real_target: &Path, mountpoint: &Path) -> io::Result<()> {
        let plan = Plan::new(real_target, mountpoint)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
        let mut spins = 0u32;
        loop {
            if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) } == 0 {
                break;
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINVAL) || std::time::Instant::now() >= deadline {
                return Err(err);
            }
            spins += 1;
            if spins <= 16 {
                std::thread::yield_now();
            } else {
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
        }
        unsafe { plan.finish_after_unshare() }
    }

    /// Canonical mountpoint for target directories under `cache`, if
    /// unprivileged user+mount namespaces are permitted on this kernel and
    /// `JATO_MOUNT != 0`. Probed once per cache directory.
    pub fn available(cache: &Path) -> Option<PathBuf> {
        if std::env::var_os("JATO_MOUNT").is_some_and(|v| v == "0") {
            return None;
        }
        static PROBED: OnceLock<Mutex<HashMap<PathBuf, Option<PathBuf>>>> = OnceLock::new();
        let table = PROBED.get_or_init(|| Mutex::new(HashMap::new()));
        if let Some(cached) = table.lock().unwrap_or_else(|e| e.into_inner()).get(cache) {
            return cached.clone();
        }
        let result = probe(cache);
        table
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(cache.to_owned(), result.clone());
        result
    }

    fn probe(cache: &Path) -> Option<PathBuf> {
        let mp = cache.join("mnt").join("target");
        let src = cache
            .join("mnt")
            .join(format!("probe-{}", std::process::id()));
        let plan = Plan::new(&src, &mp).ok()?;
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            let _ = fs::remove_dir(&src);
            return None;
        }
        if pid == 0 {
            let ok = unsafe { plan.enter() }.is_ok();
            unsafe { libc::_exit(if ok { 0 } else { 1 }) };
        }
        let mut status: libc::c_int = 0;
        loop {
            let r = unsafe { libc::waitpid(pid, &raw mut status, 0) };
            if r < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        let _ = fs::remove_dir(&src);
        let ok = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
        ok.then_some(mp)
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::*;
    use std::io;
    use std::process::Command;

    pub struct Plan;
    impl Plan {
        pub fn new(_: &Path, _: &Path) -> io::Result<Self> {
            Err(io::Error::from(io::ErrorKind::Unsupported))
        }
        pub unsafe fn enter(&self) -> io::Result<()> {
            Err(io::Error::from(io::ErrorKind::Unsupported))
        }
    }
    pub fn attach(_: &mut Command, _: &Path, _: &Path) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
    pub fn enter_for_exec(_: &Path, _: &Path) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
    pub fn available(_: &Path) -> Option<PathBuf> {
        None
    }
}

pub use imp::{Plan, attach, available, enter_for_exec};

/// Replace a leading `real_target` path prefix (at a component boundary) in
/// `path_bytes` with `mountpoint`.
fn replace_prefix(path_bytes: &[u8], real_target: &[u8], mountpoint: &[u8]) -> Option<Vec<u8>> {
    let real = real_target.strip_suffix(b"/").unwrap_or(real_target);
    let mp = mountpoint.strip_suffix(b"/").unwrap_or(mountpoint);
    if real.is_empty() || !path_bytes.starts_with(real) {
        return None;
    }
    let rest = &path_bytes[real.len()..];
    if !rest.is_empty() && rest[0] != b'/' {
        return None;
    }
    let mut out = Vec::with_capacity(mp.len() + rest.len());
    out.extend_from_slice(mp);
    out.extend_from_slice(rest);
    Some(out)
}

/// Rewrite a rustc target-directory argument (or value following `--out-dir`,
/// `--extern`, or `-L`) to point at `mountpoint` instead of `real_target`.
///
/// `prev` is the immediately preceding argument, if any.
pub fn rewrite_arg(
    prev: Option<&OsStr>,
    arg: &OsStr,
    real_target: &Path,
    mountpoint: &Path,
) -> Option<OsString> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let real = real_target.as_os_str().as_bytes();
    let mp = mountpoint.as_os_str().as_bytes();
    let bytes = arg.as_bytes();
    let prev_b = prev.map(OsStr::as_bytes);

    let rewrite_at = |prefix_len: usize| -> Option<OsString> {
        let replaced = replace_prefix(&bytes[prefix_len..], real, mp)?;
        let mut out = Vec::with_capacity(prefix_len + replaced.len());
        out.extend_from_slice(&bytes[..prefix_len]);
        out.extend_from_slice(&replaced);
        Some(OsString::from_vec(out))
    };

    if bytes.starts_with(b"--out-dir=") {
        return rewrite_at(b"--out-dir=".len());
    }
    if bytes.starts_with(b"-Cincremental=") {
        return rewrite_at(b"-Cincremental=".len());
    }
    if bytes.starts_with(b"--codegen=incremental=") {
        return rewrite_at(b"--codegen=incremental=".len());
    }
    if bytes.starts_with(b"--extern=") {
        let rest = &bytes[b"--extern=".len()..];
        let eq = memchr::memchr(b'=', rest)?;
        return rewrite_at(b"--extern=".len() + eq + 1);
    }
    if bytes.starts_with(b"-L") && bytes.len() > 2 {
        let rest = &bytes[2..];
        for kind in [
            &b"dependency="[..],
            b"crate=",
            b"native=",
            b"framework=",
            b"all=",
        ] {
            if rest.starts_with(kind) {
                return rewrite_at(2 + kind.len());
            }
        }
        return rewrite_at(2);
    }
    if prev_b == Some(b"--out-dir") {
        return rewrite_at(0);
    }
    if matches!(prev_b, Some(b"-C" | b"--codegen")) && bytes.starts_with(b"incremental=") {
        return rewrite_at(b"incremental=".len());
    }
    if prev_b == Some(b"--extern") {
        let eq = memchr::memchr(b'=', bytes)?;
        return rewrite_at(eq + 1);
    }
    if prev_b == Some(b"-L") {
        for kind in [
            &b"dependency="[..],
            b"crate=",
            b"native=",
            b"framework=",
            b"all=",
        ] {
            if bytes.starts_with(kind) {
                return rewrite_at(kind.len());
            }
        }
        return rewrite_at(0);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_target_dir_flags_without_touching_unrelated_paths() {
        let real = Path::new("/work/a/target");
        let mp = Path::new("/cache/mnt/target");
        let rw = |prev: Option<&str>, arg: &str| {
            rewrite_arg(prev.map(OsStr::new), OsStr::new(arg), real, mp)
                .map(|s| s.to_string_lossy().into_owned())
        };
        assert_eq!(
            rw(None, "--out-dir=/work/a/target/debug/deps").as_deref(),
            Some("--out-dir=/cache/mnt/target/debug/deps")
        );
        assert_eq!(
            rw(Some("--out-dir"), "/work/a/target/debug/deps").as_deref(),
            Some("/cache/mnt/target/debug/deps")
        );
        assert_eq!(
            rw(
                Some("--extern"),
                "foo=/work/a/target/debug/deps/libfoo-123.rlib"
            )
            .as_deref(),
            Some("foo=/cache/mnt/target/debug/deps/libfoo-123.rlib")
        );
        assert_eq!(
            rw(None, "--extern=foo=/work/a/target/debug/deps/libfoo.rlib").as_deref(),
            Some("--extern=foo=/cache/mnt/target/debug/deps/libfoo.rlib")
        );
        assert_eq!(
            rw(Some("-L"), "dependency=/work/a/target/debug/deps").as_deref(),
            Some("dependency=/cache/mnt/target/debug/deps")
        );
        assert_eq!(
            rw(None, "-Lnative=/work/a/target/debug/build/x/out").as_deref(),
            Some("-Lnative=/cache/mnt/target/debug/build/x/out")
        );
        assert_eq!(
            rw(Some("-C"), "incremental=/work/a/target/debug/incremental").as_deref(),
            Some("incremental=/cache/mnt/target/debug/incremental")
        );
        assert_eq!(
            rw(None, "-Cincremental=/work/a/target/debug/incremental").as_deref(),
            Some("-Cincremental=/cache/mnt/target/debug/incremental")
        );
        assert_eq!(
            rw(
                Some("--codegen"),
                "incremental=/work/a/target/debug/incremental"
            )
            .as_deref(),
            Some("incremental=/cache/mnt/target/debug/incremental")
        );
        assert_eq!(
            rw(
                None,
                "--codegen=incremental=/work/a/target/debug/incremental"
            )
            .as_deref(),
            Some("--codegen=incremental=/cache/mnt/target/debug/incremental")
        );
        // Boundary safety: must not rewrite `/work/a/target2`.
        assert_eq!(rw(Some("--out-dir"), "/work/a/target2/debug/deps"), None);
        // Non-target flags are untouched.
        assert_eq!(rw(Some("--extern"), "std=/usr/lib/libstd.rlib"), None);
        assert_eq!(rw(None, "/work/a/target/debug/build/x/out/gen.rs"), None);
    }

    #[test]
    fn available_probe_never_panics() {
        let dir = tempfile::tempdir().unwrap();
        let _ = available(dir.path());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn enter_for_exec_succeeds_immediately_after_joining_worker_threads() {
        let dir = tempfile::tempdir().unwrap();
        let Some(mp) = available(dir.path()) else {
            return;
        };
        let real = dir.path().join("target");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("sentinel"), b"ok").unwrap();
        for iter in 0..40 {
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0, "fork failed");
            if pid == 0 {
                // Mirror give_saved_state / place_saved joining worker threads
                // immediately before Run::Continued enters the mount namespace.
                std::thread::scope(|s| {
                    for _ in 0..4 {
                        s.spawn(|| std::hint::black_box(1 + 1));
                    }
                });
                let ok = enter_for_exec(&real, &mp).is_ok()
                    && std::fs::read(mp.join("sentinel")).ok().as_deref() == Some(b"ok".as_slice());
                unsafe { libc::_exit(if ok { 0 } else { 1 }) };
            }
            let mut status: libc::c_int = 0;
            loop {
                let r = unsafe { libc::waitpid(pid, &raw mut status, 0) };
                if r < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                break;
            }
            assert!(
                libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
                "enter_for_exec failed after thread::scope on iter {iter} (status {status})"
            );
        }
    }
}
