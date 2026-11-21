//! Native linkers can read startup objects, implicit archives, scripts and
//! plugins which rustc does not put in its .d file. On Linux, learn the file
//! lookups of the original compiler during eligible links and archive builds. A later hit
//! checks both successful reads and unsuccessful search candidates.
use crate::fingerprint::{field, recent, stamp};
use crate::invocation::Invocation;
use crate::store::Digest;
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
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::SystemTime;

const MAX_PATHS: usize = 4096;
const MAX_LINE: usize = 1024 * 1024;

#[derive(Default)]
struct Access {
    present: bool,
    missing: bool,
    not_directory: bool,
    read: bool,
    list: bool,
    wrote: bool,
    read_write_before_creation: bool,
    created_dir: bool,
    exclusive: bool,
    opened_as: Option<PathBuf>,
}

#[derive(Default)]
pub struct Observations {
    paths: BTreeMap<PathBuf, Access>,
    images: BTreeSet<PathBuf>,
    shared_listing: Option<(PathBuf, Digest)>,
    hardlinks: Vec<(PathBuf, PathBuf)>,
}

fn crate_directory(name: &OsStr, krate: &str) -> bool {
    name.as_bytes()
        .strip_prefix(krate.as_bytes())
        .and_then(|s| s.strip_prefix(b"-"))
        .is_some_and(|id| !id.is_empty() && id.iter().all(u8::is_ascii_alphanumeric))
}

// Only learn a native result from an incremental session starting from
// scratch. An active session takes the direct rustc path, without tracing.
pub fn fresh_incremental(dir: &Path, krate: &str) -> bool {
    let Some(parent) = dir.parent() else {
        return false;
    };
    if fs::canonicalize(parent).ok().as_deref() != Some(parent) {
        return false;
    }
    match fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => fs::read_dir(dir).is_ok_and(|entries| {
            entries.enumerate().all(|(index, e)| {
                index < 16384 && e.is_ok_and(|e| !crate_directory(&e.file_name(), krate))
            })
        }),
        Err(e) => e.kind() == io::ErrorKind::NotFound,
        _ => false,
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Manifest {
    missing: Vec<(PathBuf, bool)>,
    lookup: Vec<PathBuf>,
    read: Vec<PathBuf>,
    listed: Vec<PathBuf>,
    shared_listing: Option<(PathBuf, Digest)>,
}

// Older Cargo puts dependencies and the unit's own outputs in the same
// directory. Its mtime necessarily changes. Keep the actual names, order and
// types read by rustc in a streaming digest, excluding its declared outputs.
struct SharedDirectory {
    path: PathBuf,
    outputs: Vec<PathBuf>,
    pending: Option<((u32, u32), blake3::Hasher, usize)>,
    completed: Option<Digest>,
}

impl SharedDirectory {
    fn record(&mut self, pid: u32, args: &str, result: &str) -> Result<()> {
        let (fd, batch) = args.split_once(", ").context("missing directory batch")?;
        let number = fd
            .split_once('<')
            .context("unknown directory fd")?
            .0
            .parse()?;
        let (owner, hash, count) = self
            .pending
            .get_or_insert_with(|| ((pid, number), blake3::Hasher::new(), 0));
        ensure!(
            *owner == (pid, number),
            "overlapping shared directory reads"
        );
        let batch = batch
            .strip_prefix('[')
            .and_then(|s| s.rsplit_once("], ").map(|v| v.0))
            .context("undecoded shared directory entries")?;
        if result == "0" {
            ensure!(batch.is_empty(), "nonempty directory EOF");
            let digest = *hash.finalize().as_bytes();
            ensure!(
                self.completed.is_none_or(|d| d == digest),
                "shared directory contents changed"
            );
            self.completed = Some(digest);
            self.pending = None;
        } else {
            ensure!(!batch.is_empty(), "missing directory entries");
            for entry in batch.split("}, ") {
                *count += 1;
                ensure!(*count <= 16384, "native search directory too large");
                let (_, typed) = entry
                    .split_once(", d_type=")
                    .context("missing directory type")?;
                let (kind, name) = typed
                    .split_once(", d_name=")
                    .context("missing directory name")?;
                let (name, tail) = quoted(name)?;
                ensure!(tail.is_empty() || tail == "}", "unknown directory record");
                if name.as_os_str() != "." && name.as_os_str() != ".." {
                    hash_dirent(hash, &self.outputs, name.as_os_str(), kind);
                }
            }
        }
        Ok(())
    }
}

fn hash_dirent(hash: &mut blake3::Hasher, outputs: &[PathBuf], name: &OsStr, kind: &str) {
    if outputs
        .iter()
        .all(|output| output.file_name() != Some(name))
    {
        field(hash, name.as_bytes());
        field(hash, kind.as_bytes());
    }
}

pub struct Capture {
    activity: Arc<AtomicBool>,
    reader: JoinHandle<Result<Observations>>,
}

impl Capture {
    pub fn available(inv: &Invocation, root: &Path) -> Result<()> {
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
        if !inv.executable {
            return Ok(());
        }
        let linker = inv.linker.as_deref().unwrap_or_else(|| Path::new("cc"));
        let candidate = if linker.components().count() > 1 {
            linker.to_owned()
        } else {
            let bin = root.join("lib/rustlib/x86_64-unknown-linux-gnu/bin");
            [bin.clone(), bin.join("self-contained")]
                .into_iter()
                .chain(std::env::split_paths(
                    &std::env::var_os("PATH").unwrap_or_default(),
                ))
                .map(|d| d.join(linker))
                .find(|p| p.is_file())
                .context("no native linker driver")?
        };
        ensure!(
            trusted_tool(&fs::canonicalize(&candidate)?, root)?.is_some(),
            "{} is a custom linker or launcher. --system-linker selects /usr/bin/cc",
            candidate.display()
        );
        Ok(())
    }

    pub fn command(
        compiler: &OsStr,
        args: &[OsString],
        inv: &Invocation,
    ) -> Result<(Command, Self)> {
        let (reader, writer) = io::pipe()?;
        let fd = writer.as_raw_fd();
        let mut cmd = Command::new("/usr/bin/strace");
        let dir = fs::canonicalize(
            inv.outputs[0]
                .parent()
                .context("missing output directory")?,
        )?;
        let shared = dir.ends_with("deps").then(|| SharedDirectory {
            path: dir,
            outputs: inv.outputs.clone(),
            pending: None,
            completed: None,
        });
        if shared.is_some() {
            cmd.args(["-e", "abbrev=!getdents64"]);
        }
        cmd.args([
            "-f",
            "-qq",
            "--seccomp-bpf",
            "-s",
            "4096",
            "-xx",
            "-y",
            "-e",
            "trace=%file,getdents64,%network,memfd_create,io_uring_setup",
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
        let activity = Arc::new(AtomicBool::new(false));
        let tracer = activity.clone();
        let worker = std::thread::Builder::new()
            .name("jato-link-inputs".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                let mut reader = BufReader::new(reader);
                let result = parse(&mut reader, &cwd, &tracer, shared);
                // A new strace dialect or an oversized manifest only prevents
                // caching. Drain the stream so the original compiler can finish.
                let _ = io::copy(&mut reader, &mut io::sink());
                result
            })?;
        Ok((
            cmd,
            Self {
                activity,
                reader: worker,
            },
        ))
    }

    pub fn started(&self) -> bool {
        self.activity.load(Ordering::Acquire)
    }

    pub fn finish(self) -> (bool, Result<Observations>) {
        let parsed = self
            .reader
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("native input reader stopped")));
        (self.activity.load(Ordering::Acquire), parsed)
    }
}

fn parse(
    reader: &mut impl BufRead,
    cwd: &Path,
    started: &AtomicBool,
    mut shared: Option<SharedDirectory>,
) -> Result<Observations> {
    let mut seen = Observations::default();
    let mut spawn_channels = BTreeSet::new();
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
        started.store(true, Ordering::Release); // The tracer ran a tracee. Never retry its compiler on parser errors.
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
            // strace inserts one separator before the marker even in the
            // middle of an fd argument. Join the original syscall bytes.
            let prefix = prefix.strip_suffix(' ').unwrap_or(prefix);
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
        if matches!(name, "socketpair" | "recvfrom") {
            spawn_channel(&mut spawn_channels, pid, name, args, result)?;
        } else if name == "getdents64" {
            ensure!(
                !result.starts_with('-'),
                "unsuccessful native directory read"
            );
            let path = fd_path(
                args.split_once(',')
                    .context("unrecognized directory read")?
                    .0,
            )?;
            if let Some(s) = &mut shared
                && s.path == path
            {
                s.record(pid, args, result)?;
            }
            seen.paths.entry(path).or_default().list = true;
        } else {
            seen.observe(name, args, result, cwd)?;
        }
        ensure!(
            seen.paths.len() <= MAX_PATHS,
            "native input manifest too large"
        );
    }
    ensure!(
        partial.is_empty() && started.load(Ordering::Acquire),
        "incomplete compiler trace"
    );
    if let Some(s) = shared {
        ensure!(s.pending.is_none(), "incomplete shared directory listing");
        seen.shared_listing = s.completed.map(|d| (s.path, d));
    }
    Ok(seen)
}

// std::process creates a private close-on-exec socketpair when spawning a
// linker by name. EOF tells the parent exec succeeded. Match the creating
// thread, fd and socket inode, and accept only an empty read of that channel.
fn spawn_channel(
    channels: &mut BTreeSet<(u32, String)>,
    pid: u32,
    name: &str,
    args: &str,
    result: &str,
) -> Result<()> {
    ensure!(result == "0", "external or failed native IPC");
    if name == "socketpair" {
        let pair = args
            .strip_prefix("AF_UNIX, SOCK_SEQPACKET|SOCK_CLOEXEC, 0, [")
            .and_then(|s| s.strip_suffix(']'))
            .context("unrecognized native socketpair")?;
        let (parent, _) = pair
            .split_once(", ")
            .context("incomplete native socketpair")?;
        let path = fd_path(parent)?;
        ensure!(
            path.as_os_str().as_bytes().starts_with(b"socket:[") && channels.len() < 32,
            "unknown private spawn descriptor"
        );
        channels.insert((pid, parent.to_owned()));
    } else {
        let parent = args
            .strip_suffix(", \"\", 8, 0, NULL, NULL")
            .context("native socket input")?;
        ensure!(
            channels.remove(&(pid, parent.to_owned())),
            "external native socket input"
        );
    }
    Ok(())
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
        // A NULL pathname and NULL output buffer probe kernel statx support.
        if name == "statx"
            && result.starts_with("-1 EFAULT ")
            && args.ends_with(", NULL, AT_STATX_SYNC_AS_STAT, STATX_ALL, NULL")
        {
            return Ok(());
        }
        ensure!(
            result
                .as_bytes()
                .first()
                .is_some_and(|b| b.is_ascii_digit() || *b == b'-'),
            "unrecognized syscall outcome"
        );
        let ok = !result.starts_with("-1 ");
        let absent = result.starts_with("-1 ENOENT ") || result.starts_with("-1 ENOTDIR ");
        let already = result.starts_with("-1 EEXIST ");
        let nonlink = name.starts_with("readlink") && result.starts_with("-1 EINVAL ");
        ensure!(
            ok || absent || already || nonlink,
            "unmodeled native lookup: {name} {result}"
        );
        if name == "linkat" {
            ensure!(
                ok && self.hardlinks.len() < MAX_PATHS,
                "unmodeled hard link"
            );
            let (src, rest) = argument(args, true, cwd)?;
            let (dest, flags) = argument(
                rest.strip_prefix(", ")
                    .context("missing link destination")?,
                true,
                cwd,
            )?;
            ensure!(flags == ", 0", "unmodeled hard link flags");
            self.hardlinks.push((src, dest));
            return Ok(());
        }
        let at = match name {
            "openat" | "newfstatat" | "statx" | "readlinkat" | "faccessat" | "faccessat2"
            | "unlinkat" | "mkdirat" | "fchmodat" | "fchmodat2" => true,
            "open" | "stat" | "lstat" | "access" | "readlink" | "execve" | "unlink" | "rmdir"
            | "mkdir" | "rename" | "chmod" => false,
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
        if absent {
            let not_directory = result.starts_with("-1 ENOTDIR ");
            ensure!(
                !a.missing || a.not_directory == not_directory,
                "native lookup error changed"
            );
            a.not_directory = not_directory;
            a.missing = true;
        }
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
                    a.read_write_before_creation |= flags.contains("O_RDWR")
                        && !flags.contains("O_TRUNC")
                        && !flags.contains("O_EXCL")
                        && !a.wrote;
                    a.wrote = true;
                    a.exclusive |= flags.contains("O_EXCL");
                } else {
                    a.read = true;
                    let resolved = fd_path(result)?;
                    ensure!(
                        resolved.is_absolute()
                            && a.opened_as.as_ref().is_none_or(|old| *old == resolved),
                        "input resolution changed during native compilation"
                    );
                    a.opened_as = Some(resolved);
                }
            }
            "mkdirat" | "mkdir" => {
                a.wrote = true;
                a.created_dir = true;
            }
            "unlink" | "unlinkat" | "rmdir" | "chmod" | "fchmodat" | "fchmodat2" => a.wrote = true,
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
        let mut compiled = false;
        let mut linked = false;
        for path in &self.images {
            let image = fs::canonicalize(path)?;
            if image == compiler {
                compiled = true;
            } else {
                ensure!(
                    inv.executable,
                    "external tool during native library compilation"
                );
                linked |= trusted_tool(&image, root)?
                    .with_context(|| format!("unverified native tool {}", image.display()))?;
            }
        }
        ensure!(compiled, "no stock compiler observed");
        ensure!(
            !inv.executable || linked,
            "no supported ELF linker observed"
        );
        let session = if let Some(dir) = &inv.incremental {
            ensure!(
                fs::canonicalize(dir)? == *dir,
                "redirected incremental directory"
            );
            let mut created = self.paths.iter().filter(|(path, access)| {
                access.created_dir
                    && path.parent() == Some(dir)
                    && path
                        .file_name()
                        .is_some_and(|n| crate_directory(n, &inv.name))
            });
            let (path, _) = created
                .next()
                .context("rustc reused existing incremental state")?;
            ensure!(created.next().is_none(), "multiple incremental sessions");
            Some(path.clone())
        } else {
            None
        };
        for (source, dest) in &self.hardlinks {
            ensure!(
                session.as_ref().is_some_and(|s| dest.starts_with(s))
                    && dest.extension() == Some(OsStr::new("o"))
                    && source.parent() == Some(&out)
                    && source
                        .file_name()
                        .is_some_and(|n| n.as_bytes().ends_with(b".rcgu.o"))
                    && self.paths.get(source).is_some_and(|a| a.wrote),
                "hard link outside fresh compiler work products"
            );
        }
        let folders: Vec<_> = self
            .paths
            .iter()
            .filter(|(p, a)| {
                a.created_dir
                    && (p.parent() == Some(&out) || p.parent() == Some(temp.as_path()))
                    && p.file_name().is_some_and(|n| {
                        n.as_bytes().starts_with(b"rustc")
                            || n.as_bytes().starts_with(b"rmeta")
                            || (!inv.executable
                                && p.parent() == Some(&out)
                                && n.as_bytes().starts_with(b".tmp")
                                && n.as_bytes().ends_with(b".temp-archive"))
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
            shared_listing: self.shared_listing,
        };
        for (p, a) in self.paths {
            if let (Some(dir), Some(unit)) = (&inv.incremental, &session)
                && p.starts_with(dir)
            {
                // The compiler must have successfully created this crate's
                // directory. Every read inside it must be of a directory it
                // made. Reading another unit or a prior object declines reuse.
                ensure!(
                    (p == *dir && !a.read && !a.list && (!a.wrote || a.created_dir))
                        || (p.starts_with(unit)
                            && (!a.present || a.wrote)
                            && (!a.read_write_before_creation
                                || (p.parent() == Some(unit)
                                    && p.extension() == Some(OsStr::new("lock"))))
                            && (!(a.read || a.list) || a.created_dir)
                            && a.opened_as.as_ref().is_none_or(|resolved| *resolved == p)),
                    "pre-existing incremental input {}",
                    p.display()
                );
                continue;
            }
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
            if let Some(real) = a.opened_as {
                ensure!(
                    fs::canonicalize(&p)? == real,
                    "native input was redirected during compilation"
                );
                if real.file_name().is_some_and(|n| {
                    n.as_bytes().ends_with(b".so") || n.as_bytes().windows(4).any(|w| w == b".so.")
                }) {
                    ensure!(
                        real.starts_with(root)
                            || system_file(&real) && system_owner(&real, &fs::metadata(&real)?),
                        "unverified native plugin"
                    );
                }
            }
            if a.list {
                plan.listed.push(p);
            } else if a.missing && !a.present {
                plan.missing.push((p, a.not_directory));
            } else if a.read || self.images.contains(&p) {
                plan.read.push(p);
            } else {
                plan.lookup.push(p);
            }
        }
        Ok(plan)
    }
}

// Some distributions prefix the GNU target triple or suffix an LLVM version.
// Recognize the canonical ELF executable, never just the name of a symlink.
fn trusted_tool(image: &Path, root: &Path) -> Result<Option<bool>> {
    if !image.starts_with(root)
        && !(system_file(image) && system_owner(image, &fs::metadata(image)?))
    {
        return Ok(None);
    }
    let file = image.file_name().and_then(OsStr::to_str).unwrap_or("");
    let mut name = file;
    if let Some((base, version)) = name.rsplit_once('-')
        && !version.is_empty()
        && version.bytes().all(|c| c.is_ascii_digit() || c == b'.')
    {
        name = base;
    }
    if name.starts_with("x86_64-")
        && let Some((_, last)) = name.rsplit_once('-')
    {
        name = last;
    }
    let ld = matches!(
        name,
        "ld" | "ld.bfd" | "ld.gold" | "ld.lld" | "lld" | "rust-lld" | "mold"
    );
    if !ld && !matches!(name, "gcc" | "cc" | "clang" | "collect2") {
        return Ok(None);
    }
    let mut magic = [0; 4];
    fs::File::open(image)?.read_exact(&mut magic)?;
    Ok((magic == *b"\x7fELF").then_some(ld))
}

#[cfg(target_os = "linux")]
use crate::worktree::system_owner;

#[cfg(not(target_os = "linux"))]
fn system_owner(_: &Path, m: &fs::Metadata) -> bool {
    m.uid() == 0
}

pub(crate) fn system_file(p: &Path) -> bool {
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
        outputs: &[PathBuf],
    ) -> Result<[u8; 32]> {
        let mut h = blake3::Hasher::new_derive_key("jato linux ELF input manifest v2");
        let mut parents: BTreeMap<&Path, PathBuf> = BTreeMap::new();
        let mut hashes = BTreeMap::new();
        let mut content = Vec::new();
        let mut occurrences = Vec::new();
        let mut symlinks = BTreeSet::new();
        for (p, not_directory) in &self.missing {
            let expected = if *not_directory {
                io::ErrorKind::NotADirectory
            } else {
                io::ErrorKind::NotFound
            };
            ensure!(
                matches!(fs::symlink_metadata(p),Err(e) if e.kind()==expected),
                "native search candidate changed"
            );
            field(&mut h, b"absent");
            h.update(&[u8::from(*not_directory)]);
            field(&mut h, p.as_os_str().as_bytes());
        }
        for (class, paths) in [
            (b'p', &self.lookup),
            (b'r', &self.read),
            (b'd', &self.listed),
        ] {
            for p in paths {
                let entry = fs::symlink_metadata(p)
                    .with_context(|| format!("native input {}", p.display()))?;
                let (real, m) = if entry.is_symlink() {
                    ensure!(
                        !recent(&entry, since),
                        "native symlink changed during compilation"
                    );
                    field(&mut h, fs::read_link(p)?.as_os_str().as_bytes());
                    (fs::canonicalize(p)?, fs::metadata(p)?)
                } else if let (Some(parent), Some(name)) = (p.parent(), p.file_name()) {
                    let dir = match parents.entry(parent) {
                        std::collections::btree_map::Entry::Occupied(v) => v.into_mut(),
                        std::collections::btree_map::Entry::Vacant(v) => {
                            v.insert(fs::canonicalize(parent)?)
                        }
                    };
                    (dir.join(name), entry)
                } else {
                    (fs::canonicalize(p)?, entry)
                };
                if p != &real {
                    for ancestor in p.ancestors().skip(1) {
                        if !symlinks.insert(ancestor) {
                            break;
                        }
                        let m = fs::symlink_metadata(ancestor)?;
                        ensure!(
                            !m.is_symlink() || !recent(&m, since),
                            "native path redirected during compilation"
                        );
                    }
                }
                h.update(&[class]);
                field(&mut h, p.as_os_str().as_bytes());
                field(&mut h, real.as_os_str().as_bytes());
                h.update(&m.mode().to_le_bytes());
                if m.is_dir() {
                    if class == b'd' {
                        let shared = self.shared_listing.as_ref().filter(|(dir, _)| *dir == real);
                        ensure!(
                            shared.is_some() || !recent(&m, since),
                            "native directory changed: {}",
                            real.display()
                        );
                        let mut listing = blake3::Hasher::new();
                        for (index, entry) in fs::read_dir(&real)?.enumerate() {
                            ensure!(index < 16384, "native search directory too large");
                            let e = entry?;
                            let ty = e.file_type()?;
                            let kind = if ty.is_file() {
                                "DT_REG"
                            } else if ty.is_dir() {
                                "DT_DIR"
                            } else if ty.is_symlink() {
                                "DT_LNK"
                            } else if ty.is_fifo() {
                                "DT_FIFO"
                            } else if ty.is_socket() {
                                "DT_SOCK"
                            } else if ty.is_char_device() {
                                "DT_CHR"
                            } else if ty.is_block_device() {
                                "DT_BLK"
                            } else {
                                bail!("unknown directory entry type");
                            };
                            hash_dirent(
                                &mut listing,
                                if shared.is_some() { outputs } else { &[] },
                                &e.file_name(),
                                kind,
                            );
                        }
                        ensure!(
                            stamp(&m) == stamp(&fs::metadata(&real)?),
                            "native directory changed during enumeration"
                        );
                        let digest = *listing.finalize().as_bytes();
                        ensure!(
                            shared.is_none_or(|(_, expected)| *expected == digest),
                            "shared output directory differs from compiler observation"
                        );
                        field(&mut h, &digest);
                    }
                } else if m.is_file() {
                    ensure!(
                        !recent(&m, since),
                        "native input changed during compilation"
                    );
                    if real.starts_with(root) || system_file(&real) && system_owner(&real, &m) {
                        for n in stamp(&m) {
                            h.update(&n.to_le_bytes());
                        }
                    } else if !covered.contains(&real) {
                        let index = match hashes.entry(stamp(&m)) {
                            std::collections::btree_map::Entry::Occupied(v) => *v.get(),
                            std::collections::btree_map::Entry::Vacant(v) => {
                                let index = content.len();
                                content.push(crate::content::Input::new(p, &m, since)?);
                                *v.insert(index)
                            }
                        };
                        field(&mut h, b"content");
                        occurrences.push(index);
                    }
                } else {
                    bail!("non-regular native input {}", p.display());
                }
            }
        }
        let digests = crate::content::digests(&content, 2 << 20)?;
        for index in occurrences {
            field(&mut h, &digests[index]);
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
    if let Ok(status) = fs::read_to_string("/proc/self/status") {
        for line in status.lines().filter(|l| {
            [
                "Uid:",
                "Gid:",
                "Groups:",
                "CapEff:",
                "NoNewPrivs:",
                "Seccomp:",
            ]
            .iter()
            .any(|p| l.starts_with(p))
        }) {
            field(hash, line.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_spawn_channel_only_accepts_its_own_empty_acknowledgment() {
        let mut channels = BTreeSet::new();
        let fd = r"9<\x73\x6f\x63\x6b\x65\x74\x3a\x5b\x31\x5d>";
        let pair = format!("AF_UNIX, SOCK_SEQPACKET|SOCK_CLOEXEC, 0, [{fd}, 10]");
        let read = format!(r#"{fd}, "", 8, 0, NULL, NULL"#);
        spawn_channel(&mut channels, 1, "socketpair", &pair, "0").unwrap();
        assert!(spawn_channel(&mut channels, 2, "recvfrom", &read, "0").is_err());
        assert!(spawn_channel(&mut channels, 1, "recvfrom", &read, "1").is_err());
        spawn_channel(&mut channels, 1, "recvfrom", &read, "0").unwrap();
        assert!(spawn_channel(&mut channels, 1, "recvfrom", &read, "0").is_err());
        let split = format!(
            "1 socketpair({pair}) = 0\n1 recvfrom({fd} <unfinished ...>\n1 <... recvfrom resumed>, \"\", 8, 0, NULL, NULL) = 0\n"
        );
        parse(
            &mut split.as_bytes(),
            Path::new("/"),
            &AtomicBool::new(false),
            None,
        )
        .unwrap();
    }
}
