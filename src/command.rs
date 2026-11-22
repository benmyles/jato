//! Run a compiler command, or reuse the result of an earlier run.
//!
//! Nothing here knows what the command computes. A result is the files the
//! command wrote and what it printed, and it may stand in for a later run of
//! the same command line when everything the earlier run looked at is as it
//! was. What it looked at is observed, not inferred from its arguments.
use crate::cas::{Staged, Store};
use crate::manifest::{Here, Input, Manifest, State, path_dirs, which};
use crate::observe::{self, Kind, Report, Stamp};
use crate::roots::{self, Roots};
use crate::scan::Digest;
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Instant;

/// Changes whenever a result recorded by an earlier version could be wrong
/// or unreadable for this one.
const EPOCH: &str = "jato command v1";
/// Where the workspace and target directory appear to be in compiled output.
/// A debugger started in the workspace resolves these to the real files.
const WORK_ALIAS: &str = "/proc/self/cwd";
const TARGET_ALIAS: &str = "/proc/self/cwd/target";
const CAPTURE_LIMIT: usize = 4 << 20;

pub fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
}

pub fn passthrough(mut cmd: Command) -> Result<i32> {
    Err(cmd.exec()).context("starting the original command")
}

fn original(program: &OsStr, args: &[OsString]) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd
}

/// Explanations go to stderr, or are appended to the file JATO_LOG names.
fn log(name: &str, message: impl std::fmt::Display) {
    let Some(setting) = env::var_os("JATO_LOG") else {
        return;
    };
    if setting == "debug" {
        eprintln!("jato [{name}] {message}");
    } else if Path::new(&setting).is_absolute()
        && let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&setting)
    {
        let _ = file.write_all(format!("[{name}] {message}\n").as_bytes());
    }
}

/// What the arguments say about a rustc command. None of it decides whether
/// a result is right; it only says where to look and what to leave out.
#[derive(Default)]
struct Facts {
    name: String,
    source: Option<PathBuf>,
    /// A path inside the target directory.
    hint: Option<PathBuf>,
    incremental: Option<PathBuf>,
    search: Vec<PathBuf>,
    reads_stdin: bool,
    width: Option<u16>,
    ansi: bool,
    json: bool,
    native_cpu: bool,
    keeps_real_paths: bool,
    /// Arguments left out of the command's identity, and the one whose
    /// colour setting is.
    skipped: Vec<usize>,
    json_at: Option<usize>,
}

const TAKES_VALUE: &[&str] = &[
    "--crate-name",
    "--crate-type",
    "--edition",
    "--emit",
    "--print",
    "--cfg",
    "--check-cfg",
    "-L",
    "-l",
    "--extern",
    "--sysroot",
    "--out-dir",
    "-o",
    "--explain",
    "--target",
    "-C",
    "--codegen",
    "-Z",
    "-A",
    "--allow",
    "-W",
    "--warn",
    "-D",
    "--deny",
    "-F",
    "--forbid",
    "--force-warn",
    "--cap-lints",
    "--error-format",
    "--json",
    "--color",
    "--diagnostic-width",
    "--remap-path-prefix",
    "--remap-path-scope",
    "--env-set",
];

impl Facts {
    fn parse(args: &[OsString], cwd: &Path) -> Self {
        let mut facts = Facts::default();
        let mut widths = 0;
        let mut width_tokens = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let at = i;
            let Some(arg) = args[i].to_str() else {
                i += 1;
                continue;
            };
            i += 1;
            // Split `--flag=value`, `-Cvalue` and `--flag value` alike.
            let (flag, value, wide) = if TAKES_VALUE.contains(&arg) {
                let value = args.get(i).and_then(|v| v.to_str());
                i += 1;
                (arg, value, true)
            } else if let Some((flag, value)) = arg
                .split_once('=')
                .filter(|(flag, _)| flag.starts_with("--") && TAKES_VALUE.contains(flag))
            {
                (flag, Some(value), false)
            } else if let Some(flag) = ["-C", "-L", "-Z"].iter().find(|f| arg.starts_with(**f)) {
                (*flag, Some(&arg[2..]), false)
            } else {
                if arg == "-" {
                    facts.reads_stdin = true;
                } else if !arg.starts_with('-') && arg.ends_with(".rs") && facts.source.is_none() {
                    facts.source = Some(cwd.join(arg));
                }
                continue;
            };
            let Some(value) = value else { continue };
            let tokens = if wide { vec![at, at + 1] } else { vec![at] };
            match flag {
                "--crate-name" => facts.name = value.to_owned(),
                "--out-dir" => facts.hint = Some(cwd.join(value)),
                "-o" if facts.hint.is_none() => {
                    facts.hint = cwd.join(value).parent().map(Path::to_owned)
                }
                "-L" => {
                    let dir = value.split_once('=').map_or(value, |(_, dir)| dir);
                    facts.search.push(cwd.join(dir));
                }
                "-C" | "--codegen" => {
                    if let Some(dir) = value.strip_prefix("incremental=") {
                        facts.incremental = Some(cwd.join(dir));
                        facts.skipped.extend(tokens);
                    } else if value == "target-cpu=native" {
                        facts.native_cpu = true;
                    } else if value.starts_with("instrument-coverage") {
                        facts.keeps_real_paths = true;
                    }
                }
                "--cfg" if value == "trybuild" => facts.keeps_real_paths = true,
                "--diagnostic-width" => {
                    widths += 1;
                    width_tokens = tokens;
                    facts.width = value.parse::<u16>().ok().filter(|w| *w > 0);
                }
                "--json" => {
                    facts.ansi |= value.split(',').any(|v| v == "diagnostic-rendered-ansi");
                    facts.json_at = Some(*tokens.last().unwrap());
                }
                "--error-format" => facts.json = value == "json",
                _ => (),
            }
        }
        // A repeated or malformed width is rustc's to reject, so it stays
        // part of the command.
        if widths == 1 && facts.width.is_some() {
            facts.skipped.extend(width_tokens);
        } else {
            facts.width = None;
        }
        facts.skipped.sort_unstable();
        if facts.name.is_empty() {
            facts.name = env::var("CARGO_CRATE_NAME").unwrap_or_else(|_| "rustc".into());
        }
        facts
    }

    /// A command that compiles a crate, as opposed to one that asks the
    /// compiler a question.
    fn compiles(&self) -> bool {
        self.source.is_some() && !self.reads_stdin
    }
}

/// Environment variables that are part of a command's identity. A proc
/// macro or a linker can read any variable without the compiler knowing, so
/// those that such programs are known to read are always included; a crate's
/// own `env!` uses are added from the compiler's dependency output.
fn keyed(name: &[u8]) -> bool {
    const EXACT: &[&[u8]] = &[
        b"HOME",
        b"LANG",
        b"TARGET",
        b"HOST",
        b"PROFILE",
        b"OPT_LEVEL",
        b"DEBUG",
        b"OUT_DIR",
        b"SOURCE_DATE_EPOCH",
        b"ZERO_AR_DATE",
        b"LIBRARY_PATH",
        b"LD_RUN_PATH",
        b"COMPILER_PATH",
        b"GCC_EXEC_PREFIX",
        b"CPATH",
        b"C_INCLUDE_PATH",
        b"CPLUS_INCLUDE_PATH",
        b"MACOSX_DEPLOYMENT_TARGET",
        b"SDKROOT",
        b"AR",
        b"ARFLAGS",
    ];
    const PREFIXES: &[&[u8]] = &[
        b"CARGO_",
        b"RUSTC",
        b"RUSTDOC",
        b"RUSTFLAGS",
        b"RUST_",
        b"RUSTUP_",
        b"CLIPPY_",
        b"DEP_",
        b"LC_",
        b"CC",
        b"CXX",
        b"CFLAGS",
        b"CPPFLAGS",
        b"LDFLAGS",
        b"PKG_CONFIG",
        b"MOLD_",
        b"LLD_",
    ];
    // Set by Cargo or the user for Cargo itself. They reach the compiler as
    // arguments, or say how this particular build is being run.
    const NOT: &[&[u8]] = &[
        b"CARGO_MAKEFLAGS",
        b"CARGO_PRIMARY_PACKAGE",
        b"CARGO_TARGET_DIR",
        b"CARGO_BUILD_TARGET_DIR",
        b"CARGO_BUILD_BUILD_DIR",
        b"CARGO_BUILD_JOBS",
        b"CARGO_INCREMENTAL",
        b"CARGO_LOG",
        b"RUSTC_WRAPPER",
        b"RUSTC_WORKSPACE_WRAPPER",
        b"RUSTC_LOG",
        b"RUST_LOG",
        b"RUST_BACKTRACE",
        b"RUST_LIB_BACKTRACE",
    ];
    const NOT_PREFIXES: &[&[u8]] = &[b"CARGO_TERM_", b"CARGO_PROFILE_", b"CARGO_ALIAS_"];
    if NOT.contains(&name) || NOT_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return false;
    }
    EXACT.contains(&name) || PREFIXES.iter().any(|p| name.starts_with(p))
}

fn field(hash: &mut blake3::Hasher, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

struct Job<'a> {
    program: &'a OsStr,
    args: &'a [OsString],
    cwd: PathBuf,
    facts: Facts,
    roots: Roots,
    /// Whether the compiler is told to write the roots' aliases in place of
    /// their real locations.
    remap: bool,
    path: Vec<PathBuf>,
}

impl Job<'_> {
    fn recipe(&self) -> Digest {
        let mut hash = blake3::Hasher::new_derive_key(EPOCH);
        let roots = &self.roots;
        field(&mut hash, &roots.mark_os(self.program));
        field(&mut hash, &roots.mark_os(self.cwd.as_os_str()));
        for (i, arg) in self.args.iter().enumerate() {
            if self.facts.skipped.contains(&i) {
                continue;
            }
            if self.facts.json_at == Some(i) {
                // Whether diagnostics are coloured only matters when there
                // are some. The manifest says so then.
                let plain = arg
                    .to_string_lossy()
                    .replace("diagnostic-rendered-ansi", "");
                field(&mut hash, plain.as_bytes());
                continue;
            }
            field(&mut hash, &roots.mark_os(arg));
        }
        field(&mut hash, b"environment");
        let mut vars: Vec<_> = env::vars_os()
            .filter(|(name, _)| keyed(name.as_bytes()))
            .collect();
        vars.sort();
        for (name, value) in vars {
            field(&mut hash, name.as_bytes());
            field(&mut hash, &roots.mark_os(&value));
        }
        if let Ok(extra) = env::var("JATO_KEY_ENV") {
            for name in extra.split(',').filter(|n| !n.is_empty()) {
                field(&mut hash, name.as_bytes());
                match env::var_os(name) {
                    Some(value) => field(&mut hash, &roots.mark_os(&value)),
                    None => field(&mut hash, b"\0unset"),
                }
            }
        }
        // Output files get their permissions from the mask.
        let mask = unsafe {
            let mask = libc::umask(0o077);
            libc::umask(mask);
            mask
        };
        field(&mut hash, &mask.to_le_bytes());
        field(
            &mut hash,
            if self.remap {
                b"aliases"
            } else {
                b"real paths"
            },
        );
        if self.facts.native_cpu {
            field(&mut hash, &cpu());
        }
        *hash.finalize().as_bytes()
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(self.program);
        // Another wrapper is given the compiler's path first. Options start
        // after it.
        let options = self
            .args
            .iter()
            .position(|a| a.as_bytes().starts_with(b"-"))
            .unwrap_or(0);
        cmd.args(&self.args[..options]);
        if self.remap {
            // Later options win, so the user's own remappings still apply,
            // and the target directory wins over a workspace containing it.
            let mut alias = |root: u8, alias: &str| {
                if let Some(path) = self.roots.path(root) {
                    let mut arg = OsString::from("--remap-path-prefix=");
                    arg.push(path);
                    arg.push("=");
                    arg.push(alias);
                    cmd.arg(arg);
                }
            };
            alias(roots::WORK, WORK_ALIAS);
            alias(roots::TARGET, TARGET_ALIAS);
        }
        cmd.args(&self.args[options..]);
        cmd
    }

    /// Compilations are counted apart from the questions Cargo and build
    /// scripts ask the compiler, which take no time worth saving.
    fn counter(&self, event: &'static str) -> &'static str {
        match (self.facts.compiles(), event) {
            (true, _) => event,
            (false, "hit") => "query_hit",
            (false, "miss") => "query_miss",
            (false, "stored") => "query_stored",
            (false, _) => "query_uncacheable",
        }
    }

    fn observer(&self, store: &Store) -> observe::Config {
        let mut ignore = vec![store.dir.clone()];
        ignore.extend(self.facts.incremental.clone());
        observe::Config {
            ignore,
            // rustc enumerates every search directory to find crates by
            // name. The files it settles on are opened, and recorded.
            unlisted: self.facts.search.clone(),
        }
    }
}

/// What tells one processor model from another, for code compiled to use
/// whatever this machine has.
fn cpu() -> Vec<u8> {
    let info = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let mut id = Vec::new();
    for key in ["model name", "flags", "Features", "CPU part"] {
        if let Some(line) = info.lines().find(|l| l.starts_with(key)) {
            id.extend_from_slice(line.as_bytes());
        }
    }
    id
}

fn encode_detail(code: i32, stdout: &[u8], stderr: &[u8]) -> Vec<u8> {
    let mut detail = Vec::with_capacity(stdout.len() + stderr.len() + 12);
    detail.extend_from_slice(&code.to_le_bytes());
    detail.extend_from_slice(&(stdout.len() as u64).to_le_bytes());
    detail.extend_from_slice(stdout);
    detail.extend_from_slice(stderr);
    detail
}

fn decode_detail(detail: &[u8]) -> Option<(i32, &[u8], &[u8])> {
    let code = i32::from_le_bytes(detail.get(..4)?.try_into().ok()?);
    let len = u64::from_le_bytes(detail.get(4..12)?.try_into().ok()?) as usize;
    let rest = &detail[12..];
    (len <= rest.len()).then(|| (code, &rest[..len], &rest[len..]))
}

/// Look for a recorded result that holds here and put its files in place.
fn reuse(job: &Job<'_>, store: &mut Store, recipe: &Digest) -> Result<Option<i32>> {
    let candidates = store.candidates(recipe)?;
    if candidates.is_empty() {
        log(&job.facts.name, "miss: this command has not been recorded");
        return Ok(None);
    }
    let mut here = Here::new(&job.roots, &job.path, job.facts.width, job.facts.ansi);
    let mut nearest = None;
    for candidate in &candidates {
        let manifest = match Manifest::decode(&candidate.manifest) {
            Ok(manifest) => manifest,
            Err(e) => {
                nearest.get_or_insert(format!("unreadable record: {e}"));
                continue;
            }
        };
        if let Err(why) = manifest.holds(&mut here, store) {
            nearest.get_or_insert(why);
            continue;
        }
        let Some(restored) = store.restore(&candidate.key, &job.roots)? else {
            nearest.get_or_insert("its files are no longer in the cache".into());
            continue;
        };
        let Some((code, stdout, stderr)) = decode_detail(&restored.detail) else {
            continue;
        };
        let (Some(stdout), Some(stderr)) = (job.roots.unmark(stdout), job.roots.unmark(stderr))
        else {
            continue;
        };
        for (root, path) in &manifest.removed {
            if let Some(path) = job.roots.join(*root, path) {
                let _ = fs::remove_file(path);
            }
        }
        io::stdout().write_all(&stdout)?;
        io::stderr().write_all(&stderr)?;
        store.count(job.counter("hit"), 1);
        store.count("saved_ms", restored.cost_ms);
        store.count("restored_bytes", restored.bytes);
        log(&job.facts.name, "hit");
        return Ok(Some(code));
    }
    log(
        &job.facts.name,
        format_args!(
            "miss: {} (of {} recorded)",
            nearest.unwrap_or_default(),
            candidates.len()
        ),
    );
    Ok(None)
}

/// Copy a stream through while keeping what passed, up to a limit.
fn tee(mut from: impl Read, mut to: impl Write) -> (Option<Vec<u8>>, io::Result<()>) {
    let mut kept = Some(Vec::new());
    let mut buffer = [0u8; 32 * 1024];
    loop {
        let n = match from.read(&mut buffer) {
            Ok(0) => return (kept, Ok(())),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return (None, Err(e)),
        };
        // Cargo starts dependent crates when it reads that metadata is
        // ready, so nothing may wait here.
        if let Err(e) = to.write_all(&buffer[..n]).and_then(|()| to.flush()) {
            return (None, Err(e));
        }
        if let Some(bytes) = &mut kept {
            if bytes.len() + n <= CAPTURE_LIMIT {
                bytes.extend_from_slice(&buffer[..n]);
            } else {
                kept = None;
            }
        }
    }
}

struct Ran {
    status: ExitStatus,
    stdout: Option<Vec<u8>>,
    stderr: Option<Vec<u8>>,
    report: Report,
    millis: u64,
}

fn run_observed(job: &Job<'_>, store: &Store) -> Result<Ran> {
    let started = Instant::now();
    let mut cmd = job.command();
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let (mut child, session) = observe::spawn(&mut cmd, job.observer(store))?;
    drop(cmd);
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let copy_out = std::thread::spawn(move || tee(out, io::stdout().lock()));
    let (stderr, streamed) = tee(err, io::stderr().lock());
    let (stdout, streamed_out) = copy_out
        .join()
        .unwrap_or((None, Err(io::Error::other("output thread failed"))));
    if let Err(e) = streamed.and(streamed_out) {
        // Whoever was reading has gone. The command has no reason to go on.
        let _ = child.kill();
        let _ = child.wait();
        session.finish();
        return Err(e.into());
    }
    let status = child.wait()?;
    let report = session.finish();
    Ok(Ran {
        status,
        stdout,
        stderr,
        report,
        millis: started.elapsed().as_millis() as u64,
    })
}

/// Unix sockets that libc consults for user and host names. Talking to any
/// other service means work was done where it could not be seen.
fn harmless_socket(path: &Path) -> bool {
    path.starts_with("/run/nscd")
        || path.starts_with("/var/run/nscd")
        || path.starts_with("/run/systemd/userdb")
}

/// Environment variables the compiler says the crate reads.
fn env_deps(dep_info: &[u8]) -> Vec<Vec<u8>> {
    dep_info
        .split(|b| *b == b'\n')
        .filter_map(|line| line.strip_prefix(b"# env-dep:"))
        .map(|rest| rest[..memchr::memchr(b'=', rest).unwrap_or(rest.len())].to_vec())
        .filter(|name| !name.is_empty())
        .collect()
}

/// Paths the compiler reported writing, from its JSON messages.
fn reported_artifacts(stderr: &[u8]) -> Vec<PathBuf> {
    stderr
        .split(|b| *b == b'\n')
        .filter(|line| memchr::memmem::find(line, b"\"artifact\"").is_some())
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .filter(|message| message["$message_type"] == "artifact")
        .filter_map(|message| message["artifact"].as_str().map(PathBuf::from))
        .collect()
}

/// Turn what was observed into a cache entry. An error says why this run
/// cannot be reused.
fn record(job: &Job<'_>, store: &mut Store, recipe: &Digest, ran: &Ran) -> Result<(), String> {
    let report = &ran.report;
    if let Some(why) = &report.incomplete {
        return Err(why.clone());
    }
    if report.network {
        return Err("it used the network".into());
    }
    if let Some(socket) = report.sockets.iter().find(|s| !harmless_socket(s)) {
        return Err(format!("it talked to a service at {}", socket.display()));
    }
    let (Some(stdout), Some(stderr)) = (&ran.stdout, &ran.stderr) else {
        return Err("it printed more than can be kept".into());
    };
    let roots = &job.roots;
    let mut manifest = Manifest::default();
    let mut outputs: Vec<(u8, PathBuf, PathBuf)> = Vec::new();
    let mut executed = Vec::new();
    // A compiler and its linker rewrite their outputs completely. Some
    // do so in the file an earlier build left, or after moving it aside.
    // What that file held does not change what they write.
    let artifacts = if job.facts.compiles() {
        reported_artifacts(stderr)
    } else {
        Vec::new()
    };
    let mut paths: Vec<_> = report.paths.iter().collect();
    paths.sort_by_key(|(path, _)| *path);
    for (path, access) in paths {
        let (root, rel) = roots.split(path);
        let now = fs::symlink_metadata(path);
        if access.written {
            let stale = artifacts.contains(access.origin.as_ref().unwrap_or(path));
            if access.read && !stale {
                return Err(format!("it changed {}, which it had read", path.display()));
            }
            if access.updated && !stale && now.as_ref().is_ok_and(|m| !m.is_dir()) {
                return Err(format!("it wrote into {} as it found it", path.display()));
            }
            match &now {
                Ok(meta) if meta.is_file() => outputs.push((root, rel.to_owned(), path.clone())),
                Ok(meta) if meta.is_dir() => (),
                Ok(_) => return Err(format!("it created {}, not a file", path.display())),
                // A temporary file.
                Err(_) => (),
            }
            continue;
        }
        if access.removed {
            if access.kind == Kind::Absent {
                continue;
            }
            if access.read || root == roots::ABSOLUTE {
                return Err(format!("it removed {}", path.display()));
            }
            if now.is_err() {
                manifest.removed.push((root, rel.to_owned()));
            }
            continue;
        }
        if access.created {
            continue;
        }
        let state = match access.kind {
            Kind::Absent => State::Absent,
            Kind::Dir => State::Dir(access.listing),
            Kind::File => {
                // The dynamic loader's index of installed libraries. The
                // libraries it leads to are opened, and recorded.
                if path == Path::new("/etc/ld.so.cache") {
                    continue;
                }
                let first = access.stamp.unwrap();
                let Ok(meta) = fs::metadata(path) else {
                    return Err(format!("{} went away while it ran", path.display()));
                };
                let stamp = Stamp::of(&meta);
                // rustc and Cargo give a finished file a second name, which
                // moves its change time and nothing else.
                let renamed = Stamp {
                    ctime: first.ctime,
                    ..stamp
                } == first
                    && std::os::unix::fs::MetadataExt::nlink(&meta) != access.links;
                if stamp != first && !renamed {
                    return Err(format!("{} changed while it ran", path.display()));
                }
                let digest = store
                    .digest_known(path, stamp)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                if access.executed {
                    executed.push(path.clone());
                }
                State::File(digest)
            }
            Kind::Other if access.read => {
                return Err(format!("it read {}, not a file", path.display()));
            }
            Kind::Other => continue,
        };
        manifest.inputs.push(Input {
            root,
            path: rel.to_owned(),
            link: access
                .link
                .as_ref()
                .map(|target| roots.mark_os(target.as_os_str()).into_owned()),
            state,
        });
    }
    // The kernel loads these itself, so no call of the command named them.
    let mut depth = 0;
    while let Some(program) = executed.pop() {
        let Some(loader) = crate::scan::interpreter(&program) else {
            continue;
        };
        let (root, rel) = roots.split(&loader);
        if manifest
            .inputs
            .iter()
            .any(|i| i.root == root && i.path == rel)
        {
            continue;
        }
        let Ok(meta) = fs::metadata(&loader) else {
            continue;
        };
        let digest = store
            .digest_known(&loader, Stamp::of(&meta))
            .map_err(|e| format!("{}: {e}", loader.display()))?;
        let link = fs::symlink_metadata(&loader)
            .ok()
            .filter(|m| m.file_type().is_symlink())
            .and_then(|_| fs::read_link(&loader).ok())
            .map(|target| roots.mark_os(target.as_os_str()).into_owned());
        manifest.inputs.push(Input {
            root,
            path: rel.to_owned(),
            link,
            state: State::File(digest),
        });
        depth += 1;
        if depth < 64 {
            executed.push(loader);
        }
    }
    // A name looked up on PATH can be found somewhere else under another
    // PATH, even while everything recorded above stays as it is.
    let mut names = BTreeSet::new();
    for path in report.paths.keys() {
        if let (Some(dir), Some(name)) = (path.parent(), path.file_name())
            && job.path.iter().any(|d| d == dir)
        {
            names.insert(name.to_owned());
        }
    }
    for name in names {
        let found = which(&name, &job.path).map(|p| {
            let (root, rel) = roots.split(&p);
            (root, rel.to_owned())
        });
        manifest.which.push((name.as_bytes().to_vec(), found));
    }
    if let Some(missing) = artifacts
        .iter()
        .find(|artifact| !outputs.iter().any(|(_, _, path)| path == *artifact))
    {
        return Err(format!(
            "it reported writing {}, which was not seen",
            missing.display()
        ));
    }
    for (_, _, path) in &outputs {
        if path.extension() == Some(OsStr::new("d"))
            && fs::metadata(path).is_ok_and(|m| m.len() <= 8 << 20)
            && let Ok(text) = fs::read(path)
        {
            for name in env_deps(&text) {
                let value = env::var_os(OsStr::from_bytes(&name));
                let value = value.as_ref().map(|v| roots.mark_os(v).into_owned());
                manifest.env.push((name, value));
            }
        }
    }
    let rendered = if job.facts.json {
        memchr::memmem::find(stderr, b"\"rendered\":\"").is_some()
    } else {
        !stderr.is_empty()
    };
    if rendered {
        manifest.width = Some(job.facts.width);
        manifest.ansi = Some(job.facts.ansi);
    }
    let mut staged: Vec<Staged> = Vec::with_capacity(outputs.len());
    let spellings: Vec<u8> = roots.spellings().iter().map(|(id, _)| *id).collect();
    for (root, rel, path) in &outputs {
        let output = store
            .stage(*root, rel, path, roots)
            .map_err(|e| format!("{e:#}"))?;
        for (id, named) in spellings.iter().zip(&output.names) {
            let pin = (*id, roots.path(*id).unwrap().to_owned());
            if *named && !manifest.pins.contains(&pin) {
                manifest.pins.push(pin);
            }
        }
        manifest.outputs.push((*root, rel.clone()));
        staged.push(output);
    }
    manifest.normalize();
    let key = manifest.key(recipe);
    let encoded = manifest.encode().map_err(|e| format!("{e:#}"))?;
    let detail = encode_detail(0, &roots.mark(stdout), &roots.mark(stderr));
    match store.commit(recipe, &key, &encoded, &detail, ran.millis, &mut staged) {
        Ok(true) => {
            if !manifest.pins.is_empty() {
                store.count("pinned", 1);
                log(
                    &job.facts.name,
                    format_args!(
                        "its output names {}, so it is only reused there",
                        manifest.pins[0].1.display()
                    ),
                );
            }
            Ok(())
        }
        Ok(false) => Err("it is larger than half the cache".into()),
        Err(e) => Err(format!("{e:#}")),
    }
}

/// Wait for another process that is running this same command on the same
/// source, so that its result can be reused. The lock is released when this
/// process exits, however that happens.
fn queue(store: &Store, recipe: &Digest, source: Option<Digest>) -> Option<(fs::File, bool)> {
    if env::var_os("JATO_COALESCE").is_some_and(|v| v == "0") {
        return None;
    }
    let mut hash = blake3::Hasher::new();
    hash.update(recipe);
    hash.update(&source.unwrap_or_default());
    let name = hash.finalize().to_hex();
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(store.dir.join("locks").join(&name.as_str()[..40]))
        .ok()?;
    match file.try_lock() {
        Ok(()) => Some((file, false)),
        Err(fs::TryLockError::WouldBlock) => {
            file.lock().ok()?;
            Some((file, true))
        }
        Err(_) => None,
    }
}

pub fn wrap(program: &OsStr, args: &[OsString]) -> Result<i32> {
    if env::var_os("JATO_DISABLE").is_some_and(|v| v == "1") {
        return passthrough(original(program, args));
    }
    let cwd = env::current_dir()?;
    let facts = Facts::parse(args, &cwd);
    if facts.reads_stdin
        && fs::read_link("/proc/self/fd/0").ok().as_deref() != Some(Path::new("/dev/null"))
    {
        return passthrough(original(program, args));
    }
    let hint = facts
        .hint
        .clone()
        .or_else(|| env::var_os("OUT_DIR").map(PathBuf::from));
    let roots = Roots::detect(&cwd, hint.as_deref());
    let remap = facts.compiles()
        && !facts.keeps_real_paths
        && cfg!(target_os = "linux")
        && env::var_os("JATO_REMAP").is_none_or(|v| v != "0")
        && (roots.work.is_some() || roots.target.is_some());
    let job = Job {
        program,
        args,
        path: path_dirs(&cwd),
        cwd,
        facts,
        roots,
        remap,
    };
    let mut store = match Store::open() {
        Ok(store) => store,
        Err(e) => {
            log(&job.facts.name, format_args!("no cache: {e:#}"));
            return passthrough(original(program, args));
        }
    };
    let recipe = job.recipe();
    match reuse(&job, &mut store, &recipe) {
        Ok(Some(code)) => return Ok(code),
        Ok(None) => (),
        Err(e) => log(&job.facts.name, format_args!("lookup failed: {e:#}")),
    }
    if !observe::available() {
        // Nothing can be recorded, but the command must still run as given.
        store.count("unobserved", 1);
        drop(store);
        return passthrough(original(program, args));
    }
    let source = job.facts.source.as_ref().and_then(|path| {
        store
            .digest_known(path, Stamp::of(&fs::metadata(path).ok()?))
            .ok()
    });
    let version = store.data_version();
    store.flush();
    let lock = queue(&store, &recipe, source);
    if lock.as_ref().is_some_and(|(_, waited)| *waited) || store.data_version() != version {
        match reuse(&job, &mut store, &recipe) {
            Ok(Some(code)) => {
                store.count("coalesced", 1);
                return Ok(code);
            }
            Ok(None) => (),
            Err(e) => log(&job.facts.name, format_args!("lookup failed: {e:#}")),
        }
    }
    store.count(job.counter("miss"), 1);
    let ran = match run_observed(&job, &store) {
        Ok(ran) => ran,
        Err(e) => {
            // The command may not have started at all. Run it plainly so
            // its own failure, if any, is what Cargo sees.
            log(&job.facts.name, format_args!("not observed: {e:#}"));
            drop(lock);
            drop(store);
            return passthrough(original(program, args));
        }
    };
    if ran.status.success() {
        match record(&job, &mut store, &recipe, &ran) {
            Ok(()) => {
                store.count(job.counter("stored"), 1);
                log(&job.facts.name, "stored")
            }
            Err(why) => {
                store.count(job.counter("uncacheable"), 1);
                log(&job.facts.name, format_args!("not stored: {why}"));
            }
        }
    }
    drop(lock);
    Ok(exit_code(ran.status))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn arguments_that_do_not_change_the_result_are_left_out() {
        let list = args(&[
            "--crate-name",
            "a",
            "src/lib.rs",
            "--error-format=json",
            "--json=diagnostic-rendered-ansi,artifacts",
            "--diagnostic-width=120",
            "-C",
            "incremental=/t/debug/incremental",
            "--out-dir",
            "/t/debug/deps",
            "-L",
            "dependency=/t/debug/deps",
            "-Lnative=/usr/lib",
        ]);
        let facts = Facts::parse(&list, Path::new("/w"));
        assert_eq!(facts.name, "a");
        assert_eq!(facts.source.as_deref(), Some(Path::new("/w/src/lib.rs")));
        assert_eq!(facts.width, Some(120));
        assert!(facts.ansi && facts.json && facts.compiles());
        assert_eq!(facts.skipped, [5, 6, 7]);
        assert_eq!(facts.json_at, Some(4));
        assert_eq!(facts.hint.as_deref(), Some(Path::new("/t/debug/deps")));
        assert_eq!(
            facts.search,
            [PathBuf::from("/t/debug/deps"), PathBuf::from("/usr/lib")]
        );
        assert_eq!(
            facts.incremental.as_deref(),
            Some(Path::new("/t/debug/incremental"))
        );
    }

    #[test]
    fn a_width_rustc_would_reject_stays_in_the_command() {
        for list in [
            args(&["--diagnostic-width=0", "x.rs"]),
            args(&["--diagnostic-width=80", "--diagnostic-width=90", "x.rs"]),
            args(&["--diagnostic-width", "wide", "x.rs"]),
        ] {
            let facts = Facts::parse(&list, Path::new("/w"));
            assert_eq!(facts.width, None, "{list:?}");
            assert!(facts.skipped.is_empty(), "{list:?}");
        }
        let facts = Facts::parse(
            &args(&["--diagnostic-width", "80", "x.rs"]),
            Path::new("/w"),
        );
        assert_eq!((facts.width, &facts.skipped[..]), (Some(80), &[0, 1][..]));
    }

    #[test]
    fn a_query_is_not_a_compilation() {
        let facts = Facts::parse(
            &args(&["-", "--crate-name", "___", "--print=cfg"]),
            Path::new("/w"),
        );
        assert!(facts.reads_stdin && !facts.compiles());
        assert!(!Facts::parse(&args(&["-vV"]), Path::new("/w")).compiles());
    }

    #[test]
    fn only_variables_tools_read_are_part_of_a_command() {
        for name in [
            "CARGO_PKG_NAME",
            "RUSTC_BOOTSTRAP",
            "DEP_Z_INCLUDE",
            "CC",
            "HOME",
            "LC_ALL",
        ] {
            assert!(keyed(name.as_bytes()), "{name}");
        }
        for name in [
            "CARGO_MAKEFLAGS",
            "PATH",
            "TERM",
            "RUSTC_WRAPPER",
            "CARGO_TARGET_DIR",
            "PWD",
        ] {
            assert!(!keyed(name.as_bytes()), "{name}");
        }
    }
}
