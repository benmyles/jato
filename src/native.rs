//! Native linkers can read startup objects, implicit archives, scripts and
//! plugins which rustc does not put in its .d file. On Linux, learn the file
//! lookups of an actual, unmodified nonincremental compilation. A later hit
//! checks both successful reads and unsuccessful search candidates.
use crate::fingerprint::{field, hash_file, recent, stamp};
use crate::invocation::Invocation;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::os::fd::AsRawFd;
use std::os::unix::{
    ffi::OsStrExt,
    fs::{FileTypeExt, MetadataExt},
    process::CommandExt,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::JoinHandle;
use std::time::SystemTime;

const MAX_PATHS: usize = 4096;
const MAX_LINE: usize = 1024 * 1024;

#[derive(Default)]
struct Access {
    present: bool,
    missing: bool,
    read: bool,
    list: bool,
    wrote: bool,
    created_dir: bool,
    exclusive: bool,
}

#[derive(Default)]
pub struct Observations {
    paths: BTreeMap<PathBuf, Access>,
    images: BTreeSet<PathBuf>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Manifest {
    missing: Vec<PathBuf>,
    lookup: Vec<PathBuf>,
    read: Vec<PathBuf>,
    listed: Vec<PathBuf>,
}

pub struct Capture(JoinHandle<(bool, Result<Observations>)>);

impl Capture {
    pub fn command(compiler: &OsStr, args: &[OsString]) -> Result<(Command, Self)> {
        let exe = Path::new("/usr/bin/strace");
        ensure!(
            cfg!(all(target_os = "linux", target_arch = "x86_64")) && exe.is_file(),
            "native input tracing unavailable"
        );
        let debugged = fs::read_to_string("/proc/self/status")?.lines().any(|l| {
            l.strip_prefix("TracerPid:")
                .is_some_and(|n| n.trim() != "0")
        });
        ensure!(!debugged, "compiler already under a tracer");
        let (reader, writer) = io::pipe()?;
        let fd = writer.as_raw_fd();
        let mut cmd = Command::new(exe);
        cmd.args([
            "-f",
            "-qq",
            "--seccomp-bpf",
            "-s",
            "4096",
            "-xx",
            "-y",
            "-e",
            "trace=%file,getdents64",
            "-o",
        ])
        .arg(format!("/proc/self/fd/{fd}"))
        .arg("--")
        .arg(compiler)
        .args(args);
        // Only the trace pipe gets inherited. Keep Cargo's jobserver fds and
        // the compiler's stdout/stderr exactly as the caller supplied them.
        unsafe {
            cmd.pre_exec(move || {
                if libc::fcntl(writer.as_raw_fd(), libc::F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let cwd = std::env::current_dir()?;
        let worker = std::thread::Builder::new()
            .name("jato-link-inputs".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                let mut reader = BufReader::new(reader);
                let mut started = false;
                let result = parse(&mut reader, &cwd, &mut started);
                // A new strace dialect or an oversized manifest only prevents
                // caching. Drain the stream so the original compiler can finish.
                let _ = io::copy(&mut reader, &mut io::sink());
                (started, result)
            })?;
        Ok((cmd, Self(worker)))
    }

    pub fn finish(self) -> (bool, Result<Observations>) {
        self.0
            .join()
            .unwrap_or_else(|_| (true, Err(anyhow::anyhow!("native input reader stopped"))))
    }
}

fn parse(reader: &mut impl BufRead, cwd: &Path, started: &mut bool) -> Result<Observations> {
    let mut seen = Observations::default();
    let mut partial = BTreeMap::new();
    let mut pending_bytes = 0;
    let mut line = String::new();
    loop {
        line.clear();
        if reader
            .by_ref()
            .take(MAX_LINE as u64 + 1)
            .read_line(&mut line)?
            == 0
        {
            break;
        }
        *started = true; // The tracer ran a tracee. Never retry its compiler on parser errors.
        ensure!(line.len() <= MAX_LINE, "oversized trace record");
        let (pid, call) = line
            .trim_end()
            .split_once(' ')
            .context("unrecognized trace record")?;
        let pid: u32 = pid.parse()?;
        let call = call.trim_start();
        if call.starts_with("--- ") || call.starts_with("+++ ") {
            continue;
        }
        if let Some(prefix) = call.strip_suffix("<unfinished ...>") {
            pending_bytes += prefix.len();
            ensure!(
                pending_bytes <= 2 * MAX_LINE,
                "too many unfinished native records"
            );
            ensure!(
                partial.len() < 256 && partial.insert(pid, prefix.to_owned()).is_none(),
                "overlapping trace records"
            );
            continue;
        }
        let joined;
        let call = if let Some(resumed) = call.strip_prefix("<... ") {
            let (name, rest) = resumed
                .split_once(" resumed>")
                .context("unrecognized resumed call")?;
            let prefix = partial.remove(&pid).context("missing trace record")?;
            pending_bytes -= prefix.len();
            ensure!(
                prefix.starts_with(&format!("{name}(")),
                "mismatched trace record"
            );
            joined = prefix + rest;
            &joined
        } else {
            call
        };
        let (name, tail) = call.split_once('(').context("unrecognized syscall")?;
        let (args, result) = tail.rsplit_once(") ").context("incomplete syscall")?;
        let result = result
            .trim_start()
            .strip_prefix("= ")
            .context("missing syscall result")?;
        seen.observe(name, args, result, cwd)?;
        ensure!(
            seen.paths.len() <= MAX_PATHS,
            "native input manifest too large"
        );
    }
    ensure!(partial.is_empty() && *started, "incomplete compiler trace");
    Ok(seen)
}

// With -xx strace encodes every pathname byte as four ASCII characters,
// including quotes, backslashes, spaces and newlines.
fn decode(s: &str) -> Result<PathBuf> {
    let mut bytes = Vec::with_capacity(s.len() / 4);
    let (parts, remainder) = s.as_bytes().as_chunks::<4>();
    for part in parts {
        ensure!(&part[..2] == b"\\x", "unrecognized pathname encoding");
        bytes.push(u8::from_str_radix(std::str::from_utf8(&part[2..])?, 16)?);
    }
    ensure!(remainder.is_empty(), "truncated pathname");
    Ok(Path::new(OsStr::from_bytes(&bytes)).into())
}

fn quoted(s: &str) -> Result<(PathBuf, &str)> {
    let s = s
        .trim_start()
        .strip_prefix('"')
        .context("missing pathname")?;
    let (value, rest) = s.split_once('"').context("truncated pathname")?;
    ensure!(!rest.starts_with("..."), "truncated pathname");
    Ok((decode(value)?, rest))
}

fn fd_path(fd: &str) -> Result<PathBuf> {
    decode(
        fd.split_once('<')
            .and_then(|(_, p)| p.strip_suffix('>'))
            .context("unknown directory descriptor")?,
    )
}

fn argument<'a>(args: &'a str, at: bool, cwd: &Path) -> Result<(PathBuf, &'a str)> {
    let (base, args) = if at {
        let (fd, args) = args
            .split_once(", ")
            .context("missing directory descriptor")?;
        (Some(fd), args)
    } else {
        (None, args)
    };
    let (p, rest) = quoted(args)?;
    if p.is_absolute() {
        return Ok((p, rest));
    }
    if let Some(fd) = base
        && !fd.starts_with("AT_FDCWD")
    {
        return Ok((fd_path(fd)?.join(p), rest));
    }
    Ok((cwd.join(p), rest))
}

fn process_path(path: &Path) -> bool {
    path.starts_with("/proc/self")
        || path.starts_with("/proc/thread-self")
        || path
            .strip_prefix("/proc")
            .ok()
            .and_then(|p| p.components().next())
            .is_some_and(|c| c.as_os_str().as_bytes().iter().all(u8::is_ascii_digit))
}

impl Observations {
    fn observe(&mut self, name: &str, args: &str, result: &str, cwd: &Path) -> Result<()> {
        if name == "getcwd" {
            return Ok(());
        }
        let ok = !result.starts_with("-1 ");
        let absent = result.starts_with("-1 ENOENT ") || result.starts_with("-1 ENOTDIR ");
        let already = result.starts_with("-1 EEXIST ");
        let nonlink = name.starts_with("readlink") && result.starts_with("-1 EINVAL ");
        ensure!(
            ok || absent || already || nonlink,
            "unmodeled native lookup: {name} {result}"
        );
        if name == "getdents64" {
            let path = fd_path(
                args.split_once(',')
                    .context("unrecognized directory read")?
                    .0,
            )?;
            self.paths.entry(path).or_default().list = true;
            return Ok(());
        }
        let at = match name {
            "openat" | "newfstatat" | "statx" | "readlinkat" | "faccessat" | "faccessat2"
            | "unlinkat" | "mkdirat" => true,
            "open" | "stat" | "lstat" | "access" | "readlink" | "execve" | "unlink" | "rmdir"
            | "mkdir" | "rename" => false,
            _ => bail!("unmodeled filesystem operation {name}"),
        };
        if matches!(name, "newfstatat" | "statx")
            && args.contains(", \"\",")
            && args.contains("AT_EMPTY_PATH")
            && !args.starts_with("AT_FDCWD")
        {
            return Ok(()); // fstat of an already open descriptor
        }
        let (path, rest) = argument(args, at, cwd)?;
        if path.parent() == Some(Path::new("/dev/fd"))
            && matches!(name, "openat" | "open")
            && ok
            && fs::metadata(&path).is_ok_and(|m| m.file_type().is_fifo())
        {
            return Ok(()); // rustc duplicates Cargo's inherited jobserver pipes
        }
        if process_path(&path) {
            let leaf = path
                .strip_prefix("/proc")?
                .components()
                .skip(1)
                .collect::<PathBuf>();
            ensure!(
                name != "execve"
                    && (leaf.as_os_str().is_empty()
                        || matches!(
                            leaf.to_str(),
                            Some("exe" | "maps" | "statm" | "status" | "cgroup")
                        )),
                "unmodeled process input {}",
                path.display()
            );
            return Ok(());
        }
        let a = self.paths.entry(path.clone()).or_default();
        a.present |= ok || already || nonlink;
        a.missing |= absent;
        if !ok {
            return Ok(());
        }
        match name {
            "openat" | "open" => {
                let flags = rest
                    .trim_start_matches(", ")
                    .split(',')
                    .next()
                    .unwrap_or("");
                if flags.contains("O_WRONLY")
                    || flags.contains("O_RDWR")
                    || flags.contains("O_CREAT")
                {
                    a.wrote = true;
                    a.exclusive |= flags.contains("O_EXCL");
                } else {
                    a.read = true;
                }
            }
            "mkdirat" | "mkdir" => {
                a.wrote = true;
                a.created_dir = true;
            }
            "unlink" | "unlinkat" | "rmdir" => a.wrote = true,
            "rename" => {
                a.wrote = true;
                let (dest, _) = quoted(rest.trim_start_matches(", "))?;
                self.paths.entry(cwd.join(dest)).or_default().wrote = true;
            }
            "execve" => {
                // Randomized link order, external plugins/response files, or
                // a second command interpreter cannot certify an executable.
                let argv = rest
                    .strip_prefix(", [")
                    .and_then(|s| s.split_once("], ").map(|(a, _)| a))
                    .context("unrecognized tool arguments")?;
                for arg in argv.split(", ") {
                    let (arg, tail) = quoted(arg)?;
                    ensure!(tail.is_empty(), "truncated tool arguments");
                    let b = arg.as_os_str().as_bytes();
                    ensure!(
                        !b.starts_with(b"@")
                            && !b.windows(5).any(|v| v == b"=uuid")
                            && !b.starts_with(b"--shuffle-sections")
                            && !b.starts_with(b"--random"),
                        "non-deterministic or indirect linker arguments"
                    );
                }
                self.images.insert(path);
            }
            _ => (),
        }
        Ok(())
    }

    pub fn manifest(self, inv: &Invocation, root: &Path, compiler: &Path) -> Result<Manifest> {
        let out = std::path::absolute(
            inv.outputs[0]
                .parent()
                .context("missing output directory")?,
        )?;
        let outputs: BTreeSet<_> = inv
            .outputs
            .iter()
            .map(std::path::absolute)
            .collect::<io::Result<_>>()?;
        let temp = std::env::temp_dir();
        let compiler = fs::canonicalize(compiler)?;
        let mut linked = false;
        for path in &self.images {
            let image = fs::canonicalize(path)?;
            let name = image.file_name().and_then(OsStr::to_str).unwrap_or("");
            let driver = name == "gcc"
                || name == "cc"
                || name == "clang"
                || name.starts_with("clang-")
                || name.contains("-gcc")
                || name.starts_with("gcc-");
            let ld = matches!(
                name,
                "rust-lld" | "ld" | "ld.bfd" | "ld.gold" | "ld.lld" | "lld" | "mold"
            );
            ensure!(
                image == compiler
                    || (ld || driver || name == "collect2")
                        && (image.starts_with(root)
                            || system_file(&image) && fs::metadata(&image)?.uid() == 0),
                "unverified native tool {}",
                image.display()
            );
            linked |= ld;
        }
        ensure!(linked, "no supported ELF linker observed");
        let folders: Vec<_> = self
            .paths
            .iter()
            .filter(|(p, a)| {
                a.created_dir
                    && p.parent() == Some(&out)
                    && p.file_name().is_some_and(|n| {
                        n.as_bytes().starts_with(b"rustc") || n.as_bytes().starts_with(b"rmeta")
                    })
                    && !p.exists()
            })
            .map(|(p, _)| p.clone())
            .collect();
        let mut plan = Manifest {
            missing: Vec::new(),
            lookup: Vec::new(),
            read: Vec::new(),
            listed: Vec::new(),
        };
        for (p, a) in self.paths {
            if outputs.contains(&p) || folders.iter().any(|f| p.starts_with(f)) {
                continue;
            }
            if a.wrote {
                let object = p.parent() == Some(&out)
                    && p.file_name()
                        .is_some_and(|n| n.as_bytes().ends_with(b".rcgu.o"));
                ensure!(
                    !p.exists()
                        && (object
                            || a.exclusive
                                && (p.parent() == Some(&out)
                                    || p.parent() == Some(temp.as_path()))),
                    "unexpected native output {}",
                    p.display()
                );
                continue;
            }
            // These are resource/allocator probes in the stock compiler and
            // linker. Source/assembly access to volatile files is excluded at
            // admission. The GNU build-id must be deterministic.
            if p.starts_with("/dev/fd") && !a.read && !a.wrote && !a.list {
                continue;
            }
            if p == Path::new("/proc/sys/vm/overcommit_memory")
                || p == Path::new("/sys/devices/system/cpu/online")
                || p == Path::new("/sys/kernel/mm/transparent_hugepage/enabled")
                || p.starts_with("/sys/fs/cgroup")
                    && p.file_name().is_some_and(|n| {
                        matches!(
                            n.to_str(),
                            Some(
                                "cpu.max"
                                    | "cgroup.controllers"
                                    | "cpu.cfs_quota_us"
                                    | "cpu.cfs_period_us"
                            )
                        )
                    })
                || (!a.read && !a.list && (p.starts_with("/sys") || p.starts_with("/proc")))
                || p == Path::new("/dev/urandom")
                || p == Path::new("/dev/null")
            {
                continue;
            }
            ensure!(
                !p.starts_with("/proc") && !p.starts_with("/sys"),
                "unmodeled kernel input {}",
                p.display()
            );
            ensure!(
                p == Path::new("/dev") || !p.starts_with("/dev"),
                "non-file native input {}",
                p.display()
            );
            ensure!(
                !(a.present && a.missing),
                "name changed during native compilation: {}",
                p.display()
            );
            if a.list {
                plan.listed.push(p);
            } else if a.missing && !a.present {
                plan.missing.push(p);
            } else if a.read || self.images.contains(&p) {
                plan.read.push(p);
            } else {
                plan.lookup.push(p);
            }
        }
        Ok(plan)
    }
}

fn system_file(p: &Path) -> bool {
    [
        "/usr/lib",
        "/usr/lib64",
        "/usr/libexec",
        "/usr/bin",
        "/usr/sbin",
    ]
    .iter()
    .any(|prefix| p.starts_with(prefix))
        || p == Path::new("/etc/ld.so.cache")
}

impl Manifest {
    pub fn digest(
        &self,
        root: &Path,
        since: SystemTime,
        covered: &BTreeSet<PathBuf>,
    ) -> Result<[u8; 32]> {
        let mut h = blake3::Hasher::new_derive_key("jato linux ELF input manifest v1");
        for p in &self.missing {
            ensure!(
                matches!(fs::metadata(p),Err(e) if matches!(e.kind(),io::ErrorKind::NotFound|io::ErrorKind::NotADirectory)),
                "native search candidate appeared"
            );
            field(&mut h, b"absent");
            field(&mut h, p.as_os_str().as_bytes());
        }
        for (class, paths) in [
            (b'p', &self.lookup),
            (b'r', &self.read),
            (b'd', &self.listed),
        ] {
            for p in paths {
                let real =
                    fs::canonicalize(p).with_context(|| format!("native input {}", p.display()))?;
                let m = fs::metadata(&real)?;
                h.update(&[class]);
                field(&mut h, p.as_os_str().as_bytes());
                field(&mut h, real.as_os_str().as_bytes());
                h.update(&m.mode().to_le_bytes());
                if m.is_dir() {
                    if class == b'd' {
                        ensure!(!recent(&m, since), "directory changed during compilation");
                        let mut names = fs::read_dir(&real)?
                            .take(16385)
                            .map(|e| e.map(|e| e.file_name()))
                            .collect::<io::Result<Vec<_>>>()?;
                        ensure!(names.len() <= 16384, "native search directory too large");
                        names.retain(|n| n != ".jato-staging-v1");
                        names.sort();
                        for n in names {
                            field(&mut h, n.as_bytes());
                        }
                    }
                } else if m.is_file() {
                    ensure!(
                        !recent(&m, since),
                        "native input changed during compilation"
                    );
                    if real.starts_with(root) || system_file(&real) && m.uid() == 0 {
                        for n in stamp(&m) {
                            h.update(&n.to_le_bytes());
                        }
                    } else if !covered.contains(&real) {
                        field(&mut h, &hash_file(&real, since, None)?);
                    }
                } else {
                    bail!("non-regular native input {}", p.display());
                }
            }
        }
        Ok(*h.finalize().as_bytes())
    }
}

pub fn environment(hash: &mut blake3::Hasher) {
    let mut vars: Vec<_> = std::env::vars_os().collect();
    vars.sort();
    for (k, v) in vars {
        if [b"JATO_".as_slice(), b"CODEX_", b"OLO_", b"OTEL_"]
            .iter()
            .any(|prefix| k.as_bytes().starts_with(prefix))
            || matches!(
                k.as_bytes(),
                b"_" | b"SHLVL"
                    | b"OLDPWD"
                    | b"TRACEPARENT"
                    | b"TRACESTATE"
                    | b"MAKEFLAGS"
                    | b"CARGO_MAKEFLAGS"
                    | b"MFLAGS"
            )
        {
            continue;
        }
        field(hash, k.as_bytes());
        field(hash, v.as_bytes());
    }
}
