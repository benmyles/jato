//! Run a command, or reuse the result of an earlier run.
//!
//! Nothing here knows what a command computes. A result is the files the
//! command wrote and what it printed, and it may stand in for a later run of
//! the same command line when everything the earlier run looked at is as it
//! was. What it looked at is observed, not inferred from its arguments.
use crate::cas::{Staged, Store};
use crate::facts::{Facts, keyed};
use crate::manifest::{Env, Here, Input, Manifest, Output, Place, State, Walk, path_dirs, which};
use crate::observe::{self, Kind, Report, Stamp};
use crate::roots::{self, Roots, Surroundings};
use crate::scan::Digest;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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
const EPOCH: &str = "jato command v5";
/// Where the workspace and target directory appear to be in compiled output.
/// A debugger started in the workspace resolves these to the real files.
const WORK_ALIAS: &str = "/proc/self/cwd";
const TARGET_ALIAS: &str = "/proc/self/cwd/target";
/// The descriptor a compiler is given its target directory on when that
/// directory is not inside the one it runs in, and the directory by that
/// name.
pub const HELD_FD: i32 = 219;
const HELD_ALIAS: &str = "/proc/self/fd/219";
/// The option by which gcc and clang write one directory's name for
/// another's: in debugging information, and where the source names its
/// own file.
const C_MAP: &str = "-ffile-prefix-map=";
const CAPTURE_LIMIT: usize = 4 << 20;
/// The file Cargo puts in a target directory it creates.
const TARGET_TAG: &str = "CACHEDIR.TAG";

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

/// Explanations go to stderr, or are appended to the file JATO_LOG names,
/// there with the time of day and how long this process has been running.
pub fn log(name: &str, message: impl std::fmt::Display) {
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
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let line = format!(
            "{}.{:03} +{:.1}ms [{name}] {message}\n",
            now.as_secs(),
            now.subsec_millis(),
            started().elapsed().as_secs_f64() * 1000.0
        );
        let _ = file.write_all(line.as_bytes());
    }
}

pub fn started() -> Instant {
    static STARTED: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *STARTED.get_or_init(|| {
        umask();
        Instant::now()
    })
}

/// This process's file mode creation mask. It can only be read by setting
/// it, which other threads would see, so that is done once and before any
/// are started.
pub fn umask() -> u32 {
    static MASK: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *MASK.get_or_init(|| {
        let listed = fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                let line = status.lines().find_map(|l| l.strip_prefix("Umask:"))?;
                u32::from_str_radix(line.trim(), 8).ok()
            });
        listed.unwrap_or_else(|| unsafe {
            let mask = libc::umask(0o077);
            libc::umask(mask);
            mask as u32
        })
    })
}

pub fn field(hash: &mut blake3::Hasher, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

/// A command as it was asked for.
pub struct Request {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub env: Env,
    /// Whether the descriptor a target directory can be held on is unused
    /// in the process that will start the compiler.
    pub spare: bool,
}

/// Whether descriptor `HELD_FD` is unused in this process.
pub fn descriptor_spare() -> bool {
    unsafe { libc::fcntl(HELD_FD, libc::F_GETFD) == -1 }
}

impl Request {
    pub fn current(program: &OsStr, args: &[OsString]) -> Result<Self> {
        Ok(Self {
            program: program.to_owned(),
            args: args.to_vec(),
            cwd: env::current_dir()?,
            env: env::vars_os().collect(),
            spare: descriptor_spare(),
        })
    }
}

/// Something found out about a program by trying it, remembered with the
/// cache for as long as the program's file stays as it is. An answer of no
/// is asked for again after a day: something passing may have been the
/// reason. `more` is whatever else decides the answer, and `attempt` is
/// given a directory of its own to try in.
fn learned(
    question: &str,
    programs: &[&OsStr],
    more: &[u8],
    attempt: impl FnOnce(&Path) -> Option<bool>,
) -> bool {
    use std::os::unix::fs::MetadataExt;
    const AGAIN_AFTER: u64 = 86_400;
    let Ok(cache) = crate::cas::cache_dir() else {
        return false;
    };
    let mut hash = blake3::Hasher::new_derive_key("jato learned about a program v1");
    field(&mut hash, question.as_bytes());
    for program in programs {
        field(&mut hash, program.as_bytes());
        if let Ok(meta) = fs::metadata(program) {
            for number in [meta.dev(), meta.ino(), meta.size()] {
                hash.update(&number.to_le_bytes());
            }
            hash.update(&meta.mtime().to_le_bytes());
            hash.update(&meta.mtime_nsec().to_le_bytes());
        }
    }
    field(&mut hash, more);
    let name: String = hash.finalize().as_bytes()[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let locks = cache.join("locks");
    let known = locks.join(format!("{question}-{name}"));
    let answer = || match fs::read(&known).as_deref() {
        Ok(b"yes") => Some(true),
        Ok(_) => fs::metadata(&known)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|at| at.elapsed().ok())
            .is_some_and(|age| age.as_secs() < AGAIN_AFTER)
            .then_some(false),
        Err(_) => None,
    };
    if let Some(found) = answer() {
        return found;
    }
    // Of the compilers a build starts together, one finds out and the
    // rest wait for what it learns.
    let _ = fs::create_dir_all(&locks);
    let turn = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(locks.join(format!("turn-{question}-{name}")))
        .ok();
    if let Some(turn) = &turn
        && turn.lock().is_ok()
        && let Some(found) = answer()
    {
        return found;
    }
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let scratch = locks.join(format!("probe-{}-{question}-{serial}", std::process::id()));
    let found = fs::create_dir_all(&scratch)
        .ok()
        .and_then(|()| attempt(&scratch))
        .unwrap_or(false);
    let _ = fs::remove_dir_all(&scratch);
    let _ = fs::write(&known, if found { "yes" } else { "no" });
    found
}

/// Whether the compiler this request starts still has a directory it was
/// started with on a descriptor when it comes to write: learned by having
/// it compile an empty crate into one. A program between Jato and the
/// compiler may close what it inherits, or hand the work to a process that
/// never had it.
fn keeps_descriptor(request: &Request) -> bool {
    // The compiler, after any wrappers of its own that Cargo named.
    let options = request
        .args
        .iter()
        .position(|a| a.as_bytes().starts_with(b"-"))
        .unwrap_or(0);
    let programs: Vec<&OsStr> = std::iter::once(&request.program)
        .chain(&request.args[..options])
        .map(OsString::as_os_str)
        .collect();
    // A proxy takes the compiler it starts from here.
    let toolchain = request.env.get(OsStr::new("RUSTUP_TOOLCHAIN"));
    let toolchain = toolchain.map_or(&b""[..], |t| t.as_bytes());
    learned("held", &programs, toolchain, |scratch| {
        let mut cmd = Command::new(&request.program);
        cmd.args(&request.args[..options])
            .args(["-", "--crate-name", "jato_held", "--crate-type", "lib"])
            .args(["--emit=metadata", "--out-dir", HELD_ALIAS])
            .env_clear()
            .envs(&request.env)
            .current_dir(&request.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        hold(&mut cmd, scratch).ok()?;
        let done = cmd.status().ok()?;
        let kept = done.success() && scratch.join("libjato_held.rmeta").is_file();
        log(
            "rustc",
            format_args!(
                "the compiler {} a directory it is started with",
                if kept { "keeps" } else { "does not keep" }
            ),
        );
        Some(kept)
    })
}

/// Whether a C compiler takes the option that has it write another name
/// for a directory into what it compiles.
fn maps_prefixes(request: &Request) -> bool {
    learned("maps", &[request.program.as_os_str()], b"", |_| {
        let done = Command::new(&request.program)
            .arg(format!("{C_MAP}/jato/probe=/nowhere"))
            .args(["-E", "-x", "c", "/dev/null", "-o", "/dev/null"])
            .env_clear()
            .envs(&request.env)
            .current_dir(&request.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?;
        Some(done.success())
    })
}

/// Have `cmd` start with the directory `dir` open on descriptor `HELD_FD`.
pub fn hold(cmd: &mut Command, dir: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let dir = fs::File::open(dir)?;
    unsafe {
        cmd.pre_exec(move || {
            // The copy stays open in the program that is started.
            if libc::dup2(dir.as_raw_fd(), HELD_FD) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

/// What a command printed and how it ended.
pub struct Outcome {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// What kind of command a job is, which decides the files that count as
/// the tool's own bookkeeping and the variables that are part of it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// One run of a compiler.
    Unit,
    /// A whole Cargo command.
    Build,
}

pub struct Job {
    pub request: Request,
    pub facts: Facts,
    pub roots: Roots,
    pub scope: Scope,
    /// Whether the compiler is told to write the roots' aliases in place of
    /// their real locations.
    remap: bool,
    /// Whether the compiler can be given its target directory on a
    /// descriptor: the descriptor is free, and the program has been seen to
    /// keep one.
    descriptor: bool,
    /// A compiler other than rustc, started by a build script.
    pub tool: bool,
    path: Vec<PathBuf>,
    /// Further facts that tell this command from others like it.
    pub salt: Vec<u8>,
}

impl Job {
    /// A compiler command as Cargo runs it. `target` is the target
    /// directory when whoever started the build knows it; otherwise it is
    /// found from the directory the command writes to.
    pub fn unit(request: Request, target: Option<&Path>) -> Self {
        let mut facts = Facts::parse(&request.args, &request.cwd);
        if facts.name == "rustc"
            && let Some(name) = request.env.get(OsStr::new("CARGO_CRATE_NAME"))
        {
            facts.name = name.to_string_lossy().into_owned();
        }
        let hint = facts
            .hint
            .clone()
            .or_else(|| request.env.get(OsStr::new("OUT_DIR")).map(PathBuf::from));
        // A target directory named in the environment reaches the compiler
        // with everything else in it.
        let named = ["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"]
            .iter()
            .find_map(|name| request.env.get(OsStr::new(name)))
            .map(|path| request.cwd.join(path));
        let roots = Roots::detect(&request.cwd, hint.as_deref(), target.or(named.as_deref()));
        let remap = facts.compiles()
            && !facts.keeps_real_paths
            && cfg!(target_os = "linux")
            && env::var_os("JATO_REMAP").is_none_or(|v| v != "0")
            && (roots.work.is_some() || roots.target.is_some());
        let mut job = Self {
            path: path_dirs(&request.cwd, &request.env),
            request,
            facts,
            roots,
            scope: Scope::Unit,
            remap,
            descriptor: true,
            tool: false,
            salt: Vec::new(),
        };
        // Asked only of a compilation that would be given one.
        job.descriptor = job.held().is_some()
            && job.request.spare
            && saved_state_wanted()
            && keeps_descriptor(&job.request);
        job
    }

    /// A C or C++ compiler or an assembler that a build script started.
    /// `target` is the target directory when whoever started the build
    /// knows it. Nothing is taken from the program's arguments: what it
    /// examines and writes is seen as it runs.
    pub fn tool(request: Request, target: Option<&Path>) -> Self {
        let program = Path::new(&request.program)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        // For whoever reads the log: the file it is given, if it says.
        let source = request.args.iter().find_map(|arg| {
            let arg = Path::new(arg);
            let kind = arg.extension()?.to_str()?;
            let known = ["c", "cc", "cpp", "cxx", "m", "mm", "s", "S", "asm", "o"];
            (!arg.as_os_str().as_bytes().starts_with(b"-") && known.contains(&kind))
                .then(|| arg.file_name())
                .flatten()
        });
        let name = match source {
            Some(source) => format!("{program} {}", source.to_string_lossy()),
            None => program.clone(),
        };
        let named = |variable: &str| request.env.get(OsStr::new(variable)).map(PathBuf::from);
        let roots = Roots::of_script(
            &request.cwd,
            named("CARGO_MANIFEST_DIR").as_deref(),
            named("OUT_DIR").as_deref(),
            target,
        );
        // The compiler proper, started by name, takes other options.
        let front_end = request
            .args
            .first()
            .is_some_and(|arg| arg.as_bytes().starts_with(b"-cc1"));
        let remap = crate::tools::c_family(&program)
            && !front_end
            && env::var_os("JATO_REMAP").is_none_or(|v| v != "0")
            && (roots.work.is_some() || roots.target.is_some())
            && maps_prefixes(&request);
        Self {
            path: path_dirs(&request.cwd, &request.env),
            facts: Facts {
                name,
                ..Facts::default()
            },
            request,
            roots,
            scope: Scope::Unit,
            remap,
            descriptor: false,
            tool: true,
            salt: Vec::new(),
        }
    }

    /// A Cargo command, with the workspace and target directory it uses.
    pub fn build(request: Request, name: &str, roots: Roots, salt: Vec<u8>) -> Self {
        let facts = Facts {
            name: name.to_owned(),
            ..Facts::default()
        };
        Self {
            path: path_dirs(&request.cwd, &request.env),
            request,
            facts,
            roots,
            scope: Scope::Build,
            remap: false,
            descriptor: false,
            tool: false,
            salt,
        }
    }

    pub fn name(&self) -> &str {
        &self.facts.name
    }

    fn keyed(&self, name: &[u8]) -> bool {
        match self.scope {
            Scope::Unit => keyed(name),
            // Cargo reads its configuration from variables that never reach
            // the compiler except as arguments.
            Scope::Build => {
                !matches!(
                    name,
                    b"CARGO_MAKEFLAGS"
                        | b"RUSTC_WRAPPER"
                        | b"CARGO_TARGET_DIR"
                        | b"CARGO_BUILD_TARGET_DIR"
                ) && (keyed(name)
                    || name.starts_with(b"CARGO_")
                    || name.starts_with(b"RUST")
                    || matches!(
                        name,
                        b"TERM" | b"NO_COLOR" | b"CLICOLOR" | b"CLICOLOR_FORCE"
                    ))
            }
        }
    }

    pub fn recipe(&self) -> Digest {
        let mut hash = blake3::Hasher::new_derive_key(EPOCH);
        let roots = &self.roots;
        field(&mut hash, &roots.mark_os(&self.request.program));
        field(&mut hash, &roots.mark_os(self.request.cwd.as_os_str()));
        for (i, arg) in self.request.args.iter().enumerate() {
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
        for (name, value) in &self.request.env {
            if self.keyed(name.as_bytes()) {
                field(&mut hash, name.as_bytes());
                field(&mut hash, &roots.mark_os(value));
            }
        }
        if let Some(extra) = self.request.env.get(OsStr::new("JATO_KEY_ENV")) {
            for name in extra.as_bytes().split(|b| *b == b',') {
                if name.is_empty() {
                    continue;
                }
                field(&mut hash, name);
                match self.request.env.get(OsStr::from_bytes(name)) {
                    Some(value) => field(&mut hash, &roots.mark_os(value)),
                    None => field(&mut hash, b"\0unset"),
                }
            }
        }
        // Output files get their permissions from the mask.
        field(&mut hash, &umask().to_le_bytes());
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
        field(&mut hash, &self.salt);
        *hash.finalize().as_bytes()
    }

    /// The output directory under a name that reads the same in every
    /// checkout, when the compiler saves incremental state. That state is
    /// only of use elsewhere if the compiler was told the same things
    /// there, and where its output goes is one of them. A path relative
    /// to where it runs would not do: rustc makes it absolute before it
    /// compares.
    ///
    /// Inside the directory the compiler runs in, the name goes through
    /// `/proc/self/cwd`. A target directory anywhere else is held open on
    /// a descriptor and named through that, for crates rustc writes
    /// itself: a linker would have to be trusted to keep the descriptor.
    /// Returns the argument to replace, and the directory to hold open.
    fn aliased_out_dir(&self) -> Option<(usize, OsString, Option<&Path>)> {
        let at = self.facts.out_dir_at?;
        self.facts.incremental.as_ref()?;
        if !self.remap {
            return None;
        }
        let out = self.facts.hint.as_ref()?;
        let (alias, rest, held) = match out.strip_prefix(&self.request.cwd) {
            Ok(rest) => (WORK_ALIAS, rest, None),
            Err(_) => {
                let target = self.roots.path(roots::TARGET)?;
                if self.facts.links || !self.descriptor {
                    return None;
                }
                (HELD_ALIAS, out.strip_prefix(target).ok()?, Some(target))
            }
        };
        if rest.as_os_str().is_empty() {
            return None;
        }
        let mut arg = OsString::new();
        if self.request.args[at].as_bytes().starts_with(b"--out-dir=") {
            arg.push("--out-dir=");
        }
        arg.push(Path::new(alias).join(rest));
        Some((at, arg, held))
    }

    /// The directory the compiler has to be started with on descriptor
    /// `HELD_FD`, if its output directory is named through it.
    pub fn held(&self) -> Option<&Path> {
        self.aliased_out_dir()?.2
    }

    /// Whether what the compiler saves here is of use anywhere else.
    fn state_travels(&self) -> bool {
        self.aliased_out_dir().is_some()
    }

    /// Identifies the output directory as the compiler is given it.
    fn out_dir_given(&self) -> Digest {
        let given = match (self.aliased_out_dir(), self.facts.out_dir_at) {
            (Some((_, arg, _)), _) => arg,
            (None, Some(at)) => self.request.args[at].clone(),
            (None, None) => OsString::new(),
        };
        *blake3::hash(given.as_bytes()).as_bytes()
    }

    /// The command line to run, which for a compilation names aliases for
    /// the roots.
    pub fn argv(&self) -> Vec<OsString> {
        self.command_line(
            &self
                .aliased_out_dir()
                .map(|(at, arg, _)| (at, arg))
                .into_iter()
                .collect::<Vec<_>>(),
            None,
        )
    }

    /// The command line with some arguments replaced and one more option.
    fn command_line(&self, replaced: &[(usize, OsString)], extra: Option<&str>) -> Vec<OsString> {
        let args = &self.request.args;
        let mut argv = vec![self.request.program.clone()];
        // Another wrapper is given the compiler's path first. Options start
        // after it.
        let options = args
            .iter()
            .position(|a| a.as_bytes().starts_with(b"-"))
            .unwrap_or(0);
        let arg = |i: usize| {
            replaced
                .iter()
                .find(|(at, _)| *at == i)
                .map_or_else(|| args[i].clone(), |(_, new)| new.clone())
        };
        argv.extend((0..options).map(arg));
        argv.extend(extra.map(OsString::from));
        if self.remap {
            // Later options win, so the user's own remappings still apply,
            // and the target directory wins over a workspace containing it.
            let option = if self.tool {
                C_MAP
            } else {
                "--remap-path-prefix="
            };
            for (root, alias) in [(roots::WORK, WORK_ALIAS), (roots::TARGET, TARGET_ALIAS)] {
                if let Some(path) = self.roots.path(root) {
                    let mut arg = OsString::from(option);
                    arg.push(path);
                    arg.push("=");
                    arg.push(alias);
                    argv.push(arg);
                }
            }
            // Whatever the compiler writes of where its output went reads
            // as it would have under the directory's own name.
            if self.held().is_some() {
                argv.push(format!("--remap-path-prefix={HELD_ALIAS}={TARGET_ALIAS}").into());
            }
        }
        argv.extend((options..args.len()).map(arg));
        argv
    }

    /// This compilation's command line pointed at a crate of one line and
    /// at directories of its own, to learn what the compiler writes into
    /// its saved state for these options. `other` adds an option that
    /// changes nothing but that.
    fn probe(&self, source: &Path, scratch: &Path, other: bool) -> Option<Vec<OsString>> {
        let args = &self.request.args;
        let with = |at: usize, name: &str, path: PathBuf| {
            let mut arg = OsString::new();
            let text = args[at].to_string_lossy();
            if let Some(prefix) = text.find(name).filter(|_| !name.is_empty()) {
                arg.push(&text[..prefix + name.len()]);
            }
            arg.push(path);
            (at, arg)
        };
        let out_dir = self.facts.out_dir_at?;
        let out_name = if args[out_dir].as_bytes().starts_with(b"--out-dir=") {
            "--out-dir="
        } else {
            ""
        };
        let replaced = [
            (self.facts.source_at?, source.as_os_str().to_owned()),
            with(out_dir, out_name, scratch.join("out")),
            with(
                self.facts.incremental_at?,
                "incremental=",
                scratch.join("state"),
            ),
        ];
        let extra = other.then_some("--remap-path-prefix=/nonexistent/jato/probe=/nowhere");
        let mut argv = self.command_line(&replaced, extra);
        // Nothing is wanted from the probe but the state it saves. Which
        // linker runs is not among the options rustc compares.
        argv.extend(["-C", "linker=true"].map(OsString::from));
        Some(argv)
    }

    /// Files a tool keeps for its own later runs: locks, and what rustc
    /// saves to compile incrementally. They are neither inputs nor outputs.
    fn state(&self, path: &Path) -> bool {
        let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
        if name.starts_with(".cargo-") && name.ends_with("lock")
            || name.starts_with(".package-cache")
            || name.starts_with(".global-cache")
        {
            return true;
        }
        let (root, rest) = self.roots.split(path);
        root == roots::TARGET
            && rest
                .components()
                .take(3)
                .any(|c| c.as_os_str() == "incremental")
    }

    /// Compilations are counted apart from the questions Cargo and build
    /// scripts ask the compiler, which take no time worth saving, and from
    /// whole builds.
    pub fn counter(&self, event: &'static str) -> &'static str {
        if self.tool {
            return match event {
                "hit" => "tool_hit",
                "miss" => "tool_miss",
                "stored" => "tool_stored",
                _ => "tool_uncacheable",
            };
        }
        match (self.scope, self.facts.compiles(), event) {
            (Scope::Unit, true, _) => event,
            (Scope::Unit, false, "hit") => "query_hit",
            (Scope::Unit, false, "miss") => "query_miss",
            (Scope::Unit, false, "stored") => "query_stored",
            (Scope::Unit, false, _) => "query_uncacheable",
            (Scope::Build, _, "hit") => "build_hit",
            (Scope::Build, _, "miss") => "build_miss",
            (Scope::Build, _, "stored") => "build_stored",
            (Scope::Build, ..) => "build_uncacheable",
        }
    }

    pub fn observer(&self, store: &Store) -> observe::Config {
        let mut ignore = vec![store.dir.clone()];
        ignore.extend(self.facts.incremental.clone());
        observe::Config {
            ignore,
            // rustc enumerates every search directory to find crates by
            // name. The files it settles on are opened, and recorded.
            unlisted: self.facts.search.clone(),
            state: self.facts.incremental.clone(),
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
/// For a whole build, `tree` leaves files that already hold the right bytes
/// and writes the others on several threads.
pub fn reuse(
    job: &Job,
    store: &mut Store,
    recipe: &Digest,
    tree: bool,
) -> Result<Option<(Outcome, Manifest)>> {
    store.begin_read();
    let mut forget = Vec::new();
    let found = search(job, store, recipe, tree, &mut forget);
    store.end_read();
    for key in forget {
        store.forget(&key);
    }
    if let Err(e) = &found
        && crate::cas::damaged_index(e)
    {
        crate::cas::set_aside(&store.dir);
    }
    found
}

fn search(
    job: &Job,
    store: &mut Store,
    recipe: &Digest,
    tree: bool,
    forget: &mut Vec<Digest>,
) -> Result<Option<(Outcome, Manifest)>> {
    let candidates = store.candidates(recipe)?;
    if candidates.is_empty() {
        log(job.name(), "miss: this command has not been recorded");
        if job.tool && env::var_os("JATO_LOG").is_some() {
            // Which command, with the roots as the record would have them:
            // what still names a place is what keeps it from being found.
            let line: Vec<u8> = job
                .request
                .args
                .iter()
                .flat_map(|arg| [b" ", job.roots.mark_os(arg).as_ref()].concat())
                .map(|byte| if byte == 0 { b'|' } else { byte })
                .take(600)
                .collect();
            log(
                job.name(),
                format_args!("  as:{}", String::from_utf8_lossy(&line)),
            );
        }
        return Ok(None);
    }
    let mut here = Here::new(
        &job.roots,
        &job.request.env,
        &job.path,
        job.facts.width,
        job.facts.ansi,
    );
    let mut nearest = None;
    for candidate in &candidates {
        let manifest = match Manifest::decode(&candidate.manifest) {
            // The index said this result belongs to this command. Only a
            // record that says so itself is believed.
            Ok(manifest) if manifest.key(recipe) == candidate.key => manifest,
            _ => {
                nearest.get_or_insert("its record is unreadable".to_owned());
                forget.push(candidate.key);
                continue;
            }
        };
        if let Err(why) = manifest.holds(&mut here, store) {
            nearest.get_or_insert(why);
            continue;
        }
        let mut damaged = false;
        let restored = store.restore(candidate, &manifest, &job.roots, tree, &mut damaged)?;
        let Some(restored) = restored else {
            if damaged {
                forget.push(candidate.key);
            }
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
        store.count(job.counter("hit"), 1);
        store.count("saved_ms", restored.cost_ms);
        store.count("restored_bytes", restored.bytes);
        log(job.name(), "hit");
        let outcome = Outcome {
            code,
            stdout: stdout.into_owned(),
            stderr: stderr.into_owned(),
        };
        return Ok(Some((outcome, manifest)));
    }
    log(
        job.name(),
        format_args!(
            "miss: {} (of {} recorded)",
            nearest.unwrap_or_default(),
            candidates.len()
        ),
    );
    Ok(None)
}

/// Whether the target already holds most of what this command last
/// produced. Cargo will then find most of it up to date, and watching all
/// of Cargo's own work would cost more than recording it could give back.
pub fn mostly_built(job: &Job, store: &Store, recipe: &Digest) -> bool {
    let Ok(candidates) = store.candidates(recipe) else {
        return false;
    };
    let Some(manifest) = candidates
        .first()
        .and_then(|c| Manifest::decode(&c.manifest).ok())
    else {
        return false;
    };
    // A sample is enough to tell a populated target from an empty one.
    let step = (manifest.outputs.len() / 64).max(1);
    let sample: Vec<_> = manifest.outputs.iter().step_by(step).collect();
    let present = sample
        .iter()
        .filter(|output| {
            job.roots
                .join(output.root, &output.path)
                .is_some_and(|path| path.exists())
        })
        .count();
    !sample.is_empty() && present * 2 > sample.len()
}

/// Copy a stream through while keeping what passed, up to a limit.
pub fn tee(mut from: impl Read, mut to: impl Write) -> (Option<Vec<u8>>, io::Result<()>) {
    let mut kept = Some(Vec::new());
    let mut buffer = [0u8; 32 * 1024];
    loop {
        let n = match from.read(&mut buffer) {
            Ok(0) => return (kept, Ok(())),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            // A terminal the command wrote to reports its end this way.
            Err(e) if e.raw_os_error() == Some(libc::EIO) => return (kept, Ok(())),
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

/// A command that has run to its end.
pub struct Ran {
    pub code: i32,
    pub success: bool,
    pub stdout: Option<Vec<u8>>,
    pub stderr: Option<Vec<u8>>,
    pub millis: u64,
}

/// Start `argv`, pass its output through and keep it.
pub fn run_piped(
    argv: &[OsString],
    observed: Option<(&Job, &Store)>,
    held: Option<&Path>,
) -> Result<(Ran, Report)> {
    let begun = Instant::now();
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .env(crate::jobs::HELD, "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = held {
        hold(&mut cmd, dir)?;
    }
    let (mut child, session) = match observed {
        Some((job, store)) => {
            let (child, session) = observe::spawn(&mut cmd, job.observer(store))?;
            (child, Some(session))
        }
        None => (cmd.spawn()?, None),
    };
    drop(cmd);
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let copy_out = std::thread::spawn(move || tee(out, io::stdout()));
    let (stderr, streamed) = tee(err, io::stderr());
    let (stdout, streamed_out) = copy_out
        .join()
        .unwrap_or((None, Err(io::Error::other("output thread failed"))));
    if let Err(e) = streamed.and(streamed_out) {
        // Whoever was reading has gone. The command has no reason to go on.
        let _ = child.kill();
        let _ = child.wait();
        if let Some(session) = session {
            session.finish();
        }
        return Err(e.into());
    }
    let status = child.wait()?;
    let report = session.map(observe::Session::finish).unwrap_or_default();
    Ok((
        Ran {
            code: exit_code(status),
            success: status.success(),
            stdout,
            stderr,
            millis: begun.elapsed().as_millis() as u64,
        },
        report,
    ))
}

/// Unix sockets that libc consults for user and host names, and the one a
/// build's own supervisor listens on. Talking to any other service means
/// work was done where it could not be seen.
fn harmless_socket(path: &Path) -> bool {
    path.starts_with("/run/nscd")
        || path.starts_with("/var/run/nscd")
        || path.starts_with("/run/systemd/userdb")
        || path.as_os_str().as_bytes().starts_with(b"\0jato-")
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

/// How to refer to the program with this command line when saying why a
/// result was not kept.
fn named(program: Option<&str>) -> String {
    match program {
        Some(line) if !line.is_empty() => format!("`{line}`"),
        _ => "it".to_owned(),
    }
}

/// Paths the compiler reported writing, from its JSON messages.
fn reported_artifacts(stderr: &[u8], cwd: &Path, target: Option<&Path>) -> Vec<PathBuf> {
    stderr
        .split(|b| *b == b'\n')
        .filter(|line| memchr::memmem::find(line, b"\"artifact\"").is_some())
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .filter(|message| message["$message_type"] == "artifact")
        // Named as the output directory was: through the alias of the
        // directory the compiler ran in, or of the one held open for it.
        .filter_map(|message| {
            let path = Path::new(message["artifact"].as_str()?);
            if let (Ok(rest), Some(target)) = (path.strip_prefix(HELD_ALIAS), target) {
                return Some(target.join(rest));
            }
            Some(match path.strip_prefix(WORK_ALIAS) {
                Ok(rest) => cwd.join(rest),
                Err(_) => cwd.join(path),
            })
        })
        .collect()
}

/// The environment variables Cargo recorded for a crate it compiled
/// earlier, read from the file it keeps beside the crate's fingerprint.
/// Cargo compares them with the environment before deciding that the crate
/// need not be compiled again. None if the file is not laid out as known.
fn cargo_env_deps(bytes: &[u8]) -> Option<Vec<Vec<u8>>> {
    struct Bytes<'a>(&'a [u8]);
    impl<'a> Bytes<'a> {
        fn take(&mut self, n: usize) -> Option<&'a [u8]> {
            let (head, rest) = self.0.split_at_checked(n)?;
            self.0 = rest;
            Some(head)
        }
        fn count(&mut self) -> Option<usize> {
            let n = u32::from_le_bytes(self.take(4)?.try_into().ok()?) as usize;
            (n <= self.0.len()).then_some(n)
        }
        fn text(&mut self) -> Option<&'a [u8]> {
            let n = self.count()?;
            self.take(n)
        }
    }
    let mut b = Bytes(bytes);
    let mut files = b.count()?;
    // Later versions put a marker and a version where the first path's
    // kind would be, then the count again, and can keep a checksum.
    let checksums = b.0.first() == Some(&0xff);
    if checksums {
        b.take(2)?;
        files = b.count()?;
    }
    for _ in 0..files {
        b.take(1)?;
        b.text()?;
        if checksums && b.take(1)? != [0] {
            b.take(8)?;
            b.text()?;
        }
    }
    let mut names = Vec::new();
    for _ in 0..b.count()? {
        names.push(b.text()?.to_vec());
        if b.take(1)? != [0] {
            b.text()?;
        }
    }
    b.0.is_empty().then_some(names)
}

/// The environment variables a build script asked Cargo to watch, from
/// what the script printed.
fn script_env_deps(printed: &[u8]) -> Vec<Vec<u8>> {
    printed
        .split(|b| *b == b'\n')
        .filter_map(|line| {
            line.strip_prefix(b"cargo::rerun-if-env-changed=")
                .or_else(|| line.strip_prefix(b"cargo:rerun-if-env-changed="))
        })
        .map(|name| name.trim_ascii().to_vec())
        .filter(|name| !name.is_empty())
        .collect()
}

/// What the compiler commands inside a build relied on and wrote, gathered
/// from their own records as each one finishes.
#[derive(Default)]
pub struct Parts {
    inputs: HashMap<Place, (Option<Vec<u8>>, State)>,
    /// Every value each variable had for a command that read it.
    env: BTreeMap<Vec<u8>, BTreeSet<Option<Vec<u8>>>>,
    which: BTreeMap<Vec<u8>, Option<Place>>,
    pins: BTreeSet<Place>,
    outputs: HashMap<Place, Output>,
    removed: BTreeSet<Place>,
    /// Places two commands found in different states. That is expected of
    /// what the build itself wrote in between, and nothing else.
    differing: BTreeSet<Place>,
    /// Why the build as a whole cannot be recorded, if it cannot.
    pub unusable: Option<String>,
}

impl Parts {
    /// Take in the record of one command. Its paths are relative to the
    /// roots of that command and are restated for the build's.
    pub fn add(&mut self, manifest: &Manifest, from: &Roots, to: &Roots) {
        let place = |root: u8, path: &Path| -> Option<Place> {
            let absolute = from.join(root, path)?;
            let (root, rest) = to.split(&absolute);
            Some((root, rest.to_owned()))
        };
        let text = |marked: &[u8]| -> Option<Vec<u8>> {
            Some(to.mark(&from.unmark(marked)?).into_owned())
        };
        let mut lost = false;
        for input in &manifest.inputs {
            let Some(at) = place(input.root, &input.path) else {
                lost = true;
                continue;
            };
            let link = match &input.link {
                Some(link) => match text(link) {
                    Some(link) => Some(link),
                    None => {
                        lost = true;
                        continue;
                    }
                },
                None => None,
            };
            let value = (link, input.state.clone());
            match self.inputs.get_mut(&at) {
                None => drop(self.inputs.insert(at, value)),
                Some(earlier) if *earlier == value => (),
                // One command only looked at the directory and another
                // listed it. The listing says more.
                Some(earlier)
                    if earlier.0 == value.0
                        && matches!(
                            (&earlier.1, &value.1),
                            (State::Dir(None), State::Dir(_)) | (State::Dir(_), State::Dir(None))
                        ) =>
                {
                    if earlier.1 == State::Dir(None) {
                        *earlier = value;
                    }
                }
                Some(_) => drop(self.differing.insert(at)),
            }
        }
        for (name, value) in &manifest.env {
            let value = match value {
                Some(value) => match text(value) {
                    Some(value) => Some(value),
                    None => {
                        lost = true;
                        continue;
                    }
                },
                None => None,
            };
            self.env.entry(name.clone()).or_default().insert(value);
        }
        for (name, found) in &manifest.which {
            let found = found.as_ref().and_then(|(root, path)| place(*root, path));
            self.which.insert(name.clone(), found);
        }
        // An output that names the command's own root names a place inside
        // one of the build's roots, or a place no build root covers.
        for (_, named) in &manifest.pins {
            for id in [roots::WORK, roots::TARGET] {
                if let Some(root) = to.path(id)
                    && named.starts_with(root)
                {
                    self.pins.insert((id, root.to_owned()));
                }
            }
        }
        for output in &manifest.outputs {
            match place(output.root, &output.path) {
                Some(at) => {
                    let output = Output {
                        root: at.0,
                        path: at.1.clone(),
                        ..output.clone()
                    };
                    self.outputs.insert(at, output);
                }
                None => lost = true,
            }
        }
        for (root, path) in &manifest.removed {
            match place(*root, path) {
                Some(at) => drop(self.removed.insert(at)),
                None => lost = true,
            }
        }
        if lost {
            self.unusable
                .get_or_insert("a compiler's record could not be restated".into());
        }
    }
}

/// Turn what was observed into a cache entry. An error says why this run
/// cannot be reused. `parts` holds the records of commands that ran inside
/// this one under scopes of their own.
pub fn record(
    job: &Job,
    store: &mut Store,
    recipe: &Digest,
    ran: &Ran,
    report: &Report,
    parts: Option<&Parts>,
) -> Result<Manifest, String> {
    if let Some(why) = &report.incomplete {
        return Err(why.clone());
    }
    if let Some(why) = parts.and_then(|p| p.unusable.as_ref()) {
        return Err(why.clone());
    }
    if let Some(parts) = parts
        && let Some(place) = parts
            .differing
            .iter()
            .find(|place| !parts.outputs.contains_key(place))
    {
        return Err(format!(
            "{} changed while the build ran",
            job.roots
                .join(place.0, &place.1)
                .unwrap_or_default()
                .display()
        ));
    }
    if report.network {
        return Err("it used the network".into());
    }
    if let Some((socket, caller)) = report.sockets.iter().find(|(s, _)| !harmless_socket(s)) {
        return Err(format!(
            "{} talked to a service at {}",
            named(Some(caller)),
            socket.display()
        ));
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
        reported_artifacts(stderr, &job.request.cwd, roots.path(roots::TARGET))
    } else {
        Vec::new()
    };
    // What this command made itself, here or in one of its parts, cannot
    // also be something it depended on.
    let mut made: HashSet<Place> = parts
        .map(|parts| parts.outputs.keys().cloned().collect())
        .unwrap_or_default();
    // The names found in each listed directory. Those that turn out to be
    // this command's own outputs are taken out before they are recorded.
    let mut listings: HashMap<Place, Vec<OsString>> = HashMap::new();
    let mut watched: BTreeSet<Vec<u8>> = BTreeSet::new();
    let siblings = job.scope == Scope::Unit && job.facts.compiles() && !job.facts.links;
    let mut paths: Vec<_> = report.paths.iter().collect();
    paths.sort_by_key(|(path, _)| *path);
    for (path, access) in paths {
        if job.state(path) {
            continue;
        }
        // For a Rust library the compiler takes only metadata from other
        // crates. Those named on its command line are recorded. Any other
        // it loads through them it checks against the hash they carry, so
        // it cannot have used a different one; and whether a sibling's
        // library file was there yet depends on how far the sibling's own
        // compilation had got.
        if siblings
            && !access.written
            && path
                .parent()
                .is_some_and(|dir| job.facts.search.iter().any(|d| d == dir))
            && !job.facts.externs.contains(path)
        {
            continue;
        }
        let (root, rel) = roots.split(path);
        let now = fs::symlink_metadata(path);
        if access.written {
            // Cargo compares what it is about to write with what is there,
            // and rewrites. What was there does not change what it writes.
            let stale = artifacts.contains(access.origin.as_ref().unwrap_or(path))
                || (job.scope == Scope::Build && root == roots::TARGET);
            let writer = named(access.writer.as_deref());
            if access.read && !stale {
                return Err(format!(
                    "{writer} changed {}, which had been read",
                    path.display()
                ));
            }
            if access.updated && !stale && now.as_ref().is_ok_and(|m| !m.is_dir()) {
                return Err(format!(
                    "{writer} wrote into {} as it found it",
                    path.display()
                ));
            }
            made.insert((root, rel.to_owned()));
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
        // What was there before and has been replaced by a part of this
        // command is state the command brought up to date, like a file it
        // rewrote itself.
        if access.created || made.contains(&(root, rel.to_owned())) {
            continue;
        }
        if job.scope == Scope::Build && root == roots::TARGET {
            // Which directories a target directory already has is how far
            // an earlier command got in laying it out. Cargo makes those it
            // needs. And what it remembers of the compiler's answers it
            // checks against the compiler before believing it.
            let layout = access.kind == Kind::Dir && access.listing.is_none();
            if layout || rel == Path::new(".rustc_info.json") {
                continue;
            }
        }
        // Cargo decides whether a crate or a build script is up to date
        // partly from environment variables, which it reads unseen. The
        // files that say which ones are among those it opened.
        if job.scope == Scope::Build && root == roots::TARGET && access.kind == Kind::File {
            let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
            if name.starts_with("dep-") {
                let names = fs::read(path).ok().and_then(|b| cargo_env_deps(&b));
                let Some(names) = names else {
                    return Err(format!(
                        "{} does not say which variables Cargo watches",
                        path.display()
                    ));
                };
                watched.extend(names);
            } else if matches!(name, "stdout" | "output")
                && let Ok(printed) = fs::read(path)
            {
                watched.extend(script_env_deps(&printed));
            }
        }
        let state = match access.kind {
            // Something that came to be there during the run and was then
            // read is only accounted for if a part of this command made it.
            Kind::Absent if access.appeared => {
                return Err(format!("{} appeared while it ran", path.display()));
            }
            Kind::Absent => State::Absent,
            // The compiler was told to search this directory. What it took
            // from there is recorded; that the directory exists adds nothing.
            Kind::Dir if access.listing.is_none() && job.facts.search.contains(path) => continue,
            Kind::Dir => {
                if let Some(names) = &access.listing {
                    listings.insert((root, rel.to_owned()), names.clone());
                }
                State::Dir(None)
            }
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
                // A program that was run did something whether or not
                // it may be read, and only its contents say what.
                let digest = if access.executed {
                    store.digest_known(path, stamp)
                } else {
                    store.digest_seen(path, stamp)
                }
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
    // A path leads where the links along it point. Each directory on the
    // way to an input that is a link is an input too.
    let mut walked: HashSet<PathBuf> = HashSet::new();
    let mut links = Vec::new();
    for input in &manifest.inputs {
        let Some(path) = roots.join(input.root, &input.path) else {
            continue;
        };
        for dir in path.ancestors().skip(1) {
            if dir.parent().is_none() || !walked.insert(dir.to_owned()) {
                break;
            }
            if fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink())
                && let Ok(target) = fs::read_link(dir)
            {
                links.push((dir.to_owned(), target));
            }
        }
    }
    for (dir, target) in links {
        let (root, rel) = roots.split(&dir);
        if manifest
            .inputs
            .iter()
            .any(|i| i.root == root && i.path == rel)
        {
            continue;
        }
        manifest.inputs.push(Input {
            root,
            path: rel.to_owned(),
            link: Some(roots.mark_os(target.as_os_str()).into_owned()),
            state: if dir.is_dir() {
                State::Dir(None)
            } else {
                State::Absent
            },
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
    if job.scope == Scope::Unit {
        for (_, _, path) in &outputs {
            if path.extension() == Some(OsStr::new("d"))
                && fs::metadata(path).is_ok_and(|m| m.len() <= 8 << 20)
                && let Ok(text) = fs::read(path)
            {
                for name in env_deps(&text) {
                    let value = job.request.env.get(OsStr::from_bytes(&name));
                    let value = value.map(|v| roots.mark_os(v).into_owned());
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
    }
    // Everything the parts wrote is this command's output too, and what
    // they depended on from outside is its input.
    let mut known: HashMap<Digest, &Output> = HashMap::new();
    if let Some(parts) = parts {
        let mut listed: HashSet<Place> = outputs
            .iter()
            .map(|(root, rel, _)| (*root, rel.clone()))
            .collect();
        for (place, output) in &parts.outputs {
            made.insert(place.clone());
            known.insert(output.digest, output);
            if !listed.contains(place)
                && let Some(path) = roots.join(place.0, &place.1)
                && path.is_file()
            {
                listed.insert(place.clone());
                outputs.push((place.0, place.1.clone(), path));
            }
        }
        manifest
            .inputs
            .retain(|i| !made.contains(&(i.root, i.path.clone())));
        let have: HashSet<Place> = manifest
            .inputs
            .iter()
            .map(|i| (i.root, i.path.clone()))
            .collect();
        for (place, (link, state)) in &parts.inputs {
            if !made.contains(place) && !have.contains(place) {
                manifest.inputs.push(Input {
                    root: place.0,
                    path: place.1.clone(),
                    link: link.clone(),
                    state: state.clone(),
                });
            }
        }
        // A variable a compiler read with the value this command was
        // started with is this command's to depend on. Any other value was
        // given to the compiler by Cargo, from what Cargo itself read.
        for (name, values) in &parts.env {
            let ours = job.request.env.get(OsStr::from_bytes(name));
            let ours = ours.map(|v| roots.mark_os(v).into_owned());
            if values.len() == 1 && values.contains(&ours) {
                manifest.env.push((name.clone(), ours));
            }
        }
        // A part may have looked along a PATH of its own: rustc puts its
        // own tools first for the linker it starts. What it found is among
        // the inputs. What this command's PATH finds for the name is what
        // must not change.
        for name in parts.which.keys() {
            if !manifest.which.iter().any(|(n, _)| n == name) {
                let found = which(OsStr::from_bytes(name), &job.path).map(|path| {
                    let (root, rel) = roots.split(&path);
                    (root, rel.to_owned())
                });
                manifest.which.push((name.clone(), found));
            }
        }
        manifest.pins.extend(parts.pins.iter().cloned());
        manifest.removed.extend(parts.removed.iter().cloned());
    }
    if let Some(work) = roots.path(roots::WORK) {
        let around = Surroundings::of(work, &job.request.env);
        let (walks, again) = {
            let outside: Vec<(&Input, usize, &Path)> = manifest
                .inputs
                .iter()
                .filter(|input| input.root == roots::ABSOLUTE)
                .map(|input| {
                    let (up, rest) = around.above(&input.path);
                    (input, up, rest)
                })
                .collect();
            // A name looked for in one directory above the workspace
            // after another, or in the workspace and above it, was looked
            // for from the workspace.
            let inside: HashSet<&Path> = manifest
                .inputs
                .iter()
                .filter(|input| input.root == roots::WORK)
                .map(|input| input.path.as_path())
                .collect();
            let mut levels: BTreeMap<&Path, BTreeMap<usize, &Input>> = BTreeMap::new();
            for (input, up, rest) in &outside {
                if !rest.as_os_str().is_empty() {
                    levels.entry(rest).or_default().insert(*up, input);
                }
            }
            let walks: Vec<Walk> = levels
                .iter()
                .filter(|(name, at)| at.len() + usize::from(inside.contains(*name)) > 1)
                .filter_map(|(name, at)| {
                    let last = *at.keys().next_back()?;
                    Some(Walk {
                        name: name.to_path_buf(),
                        top: work.ancestors().nth(last)?.to_owned(),
                        found: at
                            .values()
                            .filter(|input| {
                                input.state != State::Absent && !around.named(&input.path)
                            })
                            .map(|input| input.path.clone())
                            .collect(),
                    })
                })
                .collect();
            // Anything else near the workspace may have been reached from
            // it, as a crate named by `../shared` is, or by a name of its
            // own. It is recorded a second time by the way from the
            // workspace, and has to be as it was both ways.
            let walked = |rest: &Path, path: &Path| {
                walks.iter().any(|walk| {
                    walk.name == rest || walk.found.iter().any(|found| path.starts_with(found))
                })
            };
            let again: Vec<Input> = outside
                .iter()
                .filter(|(input, _, rest)| {
                    around.beside(&input.path)
                        && !around.named(&input.path)
                        && !walked(rest, &input.path)
                })
                .map(|(input, up, rest)| Input {
                    root: roots::WORK,
                    path: Surroundings::from_inside(*up, rest),
                    link: input.link.clone(),
                    state: input.state.clone(),
                })
                .collect();
            (walks, again)
        };
        manifest.walks = walks;
        manifest.inputs.extend(again);
        // What was written there was written beside this workspace, and
        // would be again nowhere else.
        let pin = (roots::WORK, work.to_owned());
        if let Some((_, _, path)) = outputs.iter().find(|(root, _, path)| {
            *root == roots::ABSOLUTE && around.beside(path) && !around.named(path)
        }) && !manifest.pins.contains(&pin)
        {
            log(
                job.name(),
                format_args!(
                    "it wrote {} beside the workspace, so this result is only reused there",
                    path.display()
                ),
            );
            manifest.pins.push(pin);
        }
    }
    if job.scope == Scope::Build {
        // Cargo marks a target directory when it creates one, and this
        // build may have found its own already made. A target directory
        // filled in from this record has to carry the mark too: backup
        // tools skip what has it, and it is how the directory is told from
        // any other.
        if let Some(tag) = roots.path(roots::TARGET).map(|t| t.join(TARGET_TAG))
            && tag.is_file()
            && !outputs.iter().any(|(_, _, path)| *path == tag)
        {
            outputs.push((roots::TARGET, PathBuf::from(TARGET_TAG), tag));
        }
        for (root, rel, path) in &outputs {
            if *root == roots::TARGET
                && matches!(
                    rel.file_name().and_then(OsStr::to_str),
                    Some("stdout" | "output")
                )
                && let Ok(printed) = fs::read(path)
            {
                watched.extend(script_env_deps(&printed));
            }
        }
        for name in watched {
            if !manifest.env.iter().any(|(n, _)| *n == name) {
                let value = job.request.env.get(OsStr::from_bytes(&name));
                let value = value.map(|v| roots.mark_os(v).into_owned());
                manifest.env.push((name, value));
            }
        }
    }
    for input in &mut manifest.inputs {
        let place = (input.root, input.path.clone());
        match (&input.state, listings.remove(&place)) {
            (State::Dir(None), Some(mut names)) => {
                // The same names are left out when the listing is compared
                // with the directory later.
                let own = roots.join(place.0, &place.1).map(|path| {
                    let written = outputs.iter().map(|(root, rel, _)| (*root, rel.as_path()));
                    crate::manifest::own_names((place.0, &place.1), &path, written, roots)
                });
                names.retain(|name| !own.as_ref().is_some_and(|own| own.contains(name)));
                input.state = State::Dir(Some(observe::listing_digest(names)));
            }
            // A part listed this directory and left out its own outputs.
            // With other parts writing here, nothing says what it saw.
            (State::Dir(Some(_)), None)
                if outputs
                    .iter()
                    .any(|(root, rel, _)| *root == place.0 && rel.parent() == Some(&place.1)) =>
            {
                return Err(format!(
                    "a compiler listed {}, where the build writes",
                    roots.join(place.0, &place.1).unwrap_or_default().display()
                ));
            }
            _ => (),
        }
    }
    let mut staged: Vec<Staged> = Vec::with_capacity(outputs.len());
    let spellings: Vec<u8> = roots.spellings().iter().map(|(id, _)| *id).collect();
    for (root, rel, path) in &outputs {
        // A file that still holds exactly what one of the parts stored
        // needs no second look.
        let meta = fs::metadata(path).ok();
        let adopted = meta
            .as_ref()
            .and_then(|meta| store.recall(&Stamp::of(meta)))
            .and_then(|digest| known.get(&digest))
            .filter(|output| !output.marked);
        if let (Some(output), Some(meta)) = (adopted, &meta) {
            manifest.outputs.push(Output {
                root: *root,
                path: rel.clone(),
                mode: std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o777,
                ..(*output).clone()
            });
            continue;
        }
        let output = store.stage(path, roots).map_err(|e| format!("{e:#}"))?;
        for (id, named) in spellings.iter().zip(&output.names) {
            let pin = (*id, roots.path(*id).unwrap().to_owned());
            if *named && !manifest.pins.contains(&pin) {
                log(
                    job.name(),
                    format_args!(
                        "{} names {}, so this result is only reused there",
                        path.display(),
                        pin.1.display()
                    ),
                );
                manifest.pins.push(pin);
            }
        }
        manifest.outputs.push(Output {
            root: *root,
            path: rel.clone(),
            digest: output.digest,
            size: output.size,
            mode: output.mode,
            marked: output.marked,
        });
        staged.push(output);
    }
    let detail = encode_detail(0, &roots.mark(stdout), &roots.mark(stderr));
    manifest.detail = *blake3::hash(&detail).as_bytes();
    manifest.normalize();
    let stored = store.commit(recipe, &manifest, &detail, ran.millis, &mut staged);
    match stored {
        Ok(true) => {
            if !manifest.pins.is_empty() {
                store.count("pinned", 1);
            }
            Ok(manifest)
        }
        Ok(false) => Err("it is larger than half the cache".into()),
        Err(e) => Err(format!("{e:#}")),
    }
}

/// Whether the compiler's saved state is kept with results and handed to
/// compilations that have none.
fn saved_state_wanted() -> bool {
    env::var_os("JATO_SAVED_STATE").is_none_or(|v| v != "0")
}

/// Identifies where a workspace and its target directory really are.
fn whereabouts(roots: &Roots) -> Digest {
    let mut hash = blake3::Hasher::new_derive_key("jato workspace and target location v1");
    for root in [roots::WORK, roots::TARGET] {
        field(
            &mut hash,
            roots
                .path(root)
                .map_or(&[][..], |p| p.as_os_str().as_bytes()),
        );
    }
    *hash.finalize().as_bytes()
}

/// The newest session rustc finished in a crate's state directory.
fn finished_session(crate_dir: &Path) -> Option<PathBuf> {
    fs::read_dir(crate_dir)
        .ok()?
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("s-") && !name.ends_with("-working") && entry.path().is_dir()
        })
        .map(|entry| entry.path())
        // The names begin with the time, in digits of a fixed width.
        .max()
}

/// Identifies the saved state of a command for one output directory.
fn state_recipe(job: &Job, recipe: &Digest) -> Digest {
    let mut hash = blake3::Hasher::new_derive_key("jato saved state v1");
    hash.update(recipe);
    hash.update(&job.out_dir_given());
    *hash.finalize().as_bytes()
}

/// How long kept state stays before a newer lot may take its place.
const KEEP_FOR: u64 = 3600;

/// Says which of rustc's sessions a compilation that just ended left.
pub enum Left<'a> {
    /// The compilation was watched: these are the names it used directly
    /// inside the directory rustc keeps state in.
    Seen(&'a std::collections::BTreeSet<OsString>),
    /// It was not: the session is the one finished since this moment.
    Since(std::time::SystemTime),
}

/// Keep what rustc saved for compiling this crate again, for compilations
/// elsewhere that have nothing to start from: the files of the session it
/// just finished. Nothing is kept if some is kept already, if that session
/// cannot be told from another's, or if it is not what this expects.
pub fn keep_state(job: &Job, store: &mut Store, recipe: &Digest, left: Left<'_>) {
    const MOST_FILES: usize = 20_000;
    const MOST_BYTES: u64 = 2 << 30;
    // State made for an output directory named as it is only suits that
    // one place, which has its own.
    if job.scope != Scope::Unit || !saved_state_wanted() || !job.state_travels() {
        return;
    }
    let Some(directory) = job.facts.incremental.as_ref() else {
        return;
    };
    let kept = state_recipe(job, recipe);
    // State is of most use next to the sources it was saved for, and those
    // move on. What is kept is replaced once it has been there a while.
    let lasts = env::var("JATO_KEEP_FOR")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(KEEP_FOR);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let earlier = store.candidates(&kept).unwrap_or_default();
    let fresh = earlier.iter().any(|candidate| {
        Manifest::decode(&candidate.manifest)
            .is_ok_and(|m| m.key(&kept) == candidate.key && now.saturating_sub(m.saved_at) < lasts)
    });
    if fresh {
        return;
    }
    let kept = (|| {
        // The one crate directory of this name that fits.
        let prefix = format!("{}-", job.name());
        let named = |name: &OsStr| name.to_string_lossy().starts_with(&prefix);
        let mut sessions: Vec<PathBuf> = match left {
            Left::Seen(names) => names
                .iter()
                .filter(|name| named(name))
                .filter_map(|name| finished_session(&directory.join(name)))
                .collect(),
            Left::Since(since) => fs::read_dir(directory)
                .ok()?
                .flatten()
                .filter(|entry| named(&entry.file_name()))
                .filter_map(|entry| finished_session(&entry.path()))
                .filter(|session| {
                    fs::metadata(session.join("dep-graph.bin"))
                        .and_then(|m| m.modified())
                        .is_ok_and(|at| at >= since)
                })
                .collect(),
        };
        let (session, None) = (sessions.pop()?, sessions.pop()) else {
            return None;
        };
        let mut files = Vec::new();
        let mut bytes = 0;
        for entry in fs::read_dir(&session).ok()? {
            let entry = entry.ok()?;
            let meta = entry.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            bytes += meta.len();
            files.push(entry.path());
        }
        if files.is_empty() || files.len() > MOST_FILES || bytes > MOST_BYTES {
            return None;
        }
        files.sort();
        let mut record = Manifest::default();
        let mut staged = Vec::with_capacity(files.len());
        for path in files {
            // As it is, whatever it holds: no root is looked for in it.
            let file = store.stage(&path, &Roots::default()).ok()?;
            let (root, rel) = job.roots.split(&path);
            record.saved.push(Output {
                root,
                path: rel.to_owned(),
                digest: file.digest,
                size: file.size,
                mode: 0o644,
                marked: false,
            });
            staged.push(file);
        }
        record.saved_in = whereabouts(&job.roots);
        record.saved_for = job.out_dir_given();
        record.saved_at = now;
        let detail = encode_detail(0, b"", b"");
        record.detail = *blake3::hash(&detail).as_bytes();
        let stored = store
            .commit(&kept, &record, &detail, 0, &mut staged)
            .ok()
            .filter(|stored| *stored)?;
        // The new state is in. What it replaces goes, apart from the
        // files the two have in common.
        let new = record.key(&kept);
        for candidate in earlier.iter().filter(|c| c.key != new) {
            store.retire(&candidate.key);
        }
        Some(stored)
    })();
    if kept.is_some() {
        store.count("state_saved", 1);
        log(
            job.name(),
            "kept what the compiler saved, for other checkouts",
        );
    }
}

/// The start of a file of rustc's saved state: a mark, a format number
/// and the compiler's version, then eight bytes that stand for the options
/// the compiler was given. Returns the two parts.
fn state_header(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    if bytes.len() < 7 || &bytes[..4] != b"RSIC" {
        return None;
    }
    let header = 7 + bytes[6] as usize;
    Some((bytes.get(..header)?, bytes.get(header..header + 8)?))
}

/// What the compiler writes for this command's options at the start of its
/// saved state, learned by having it compile a crate of one line.
///
/// rustc refuses saved state made under other options, and the real
/// location of the workspace is among them, through `--remap-path-prefix`.
/// Everything else in the state names files by their aliases, so state
/// from another checkout is good here once it carries this checkout's
/// eight bytes: the compiler still checks every result in it against the
/// sources before using it. A second compilation with one more option
/// that changes nothing else confirms that those bytes are what this
/// takes them for; if they are not, the state is not used.
fn options_mark(job: &Job, store: &Store) -> Option<(Vec<u8>, [u8; 8])> {
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let scratch = store
        .dir
        .join("locks")
        .join(format!("probe-{}-{serial}", std::process::id()));
    let found = (|| {
        fs::create_dir_all(&scratch).ok()?;
        let source = scratch.join("probe.rs");
        fs::write(
            &source,
            "//! Compiled to learn about the compiler.\n#![allow(warnings)]\nfn main() {}\n",
        )
        .ok()?;
        // Both at once: each is a compiler starting up and little more.
        let mut running = Vec::new();
        for other in [false, true] {
            let dir = scratch.join(if other { "other" } else { "same" });
            fs::create_dir_all(dir.join("out")).ok()?;
            let argv = job.probe(&source, &dir, other)?;
            let child = Command::new(&argv[0])
                .args(&argv[1..])
                .current_dir(&job.request.cwd)
                .env_clear()
                .envs(&job.request.env)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            running.push((child, dir));
        }
        let mut starts = Vec::new();
        let mut failed = false;
        for (mut child, dir) in running {
            failed |= !child.wait().is_ok_and(|status| status.success());
            if failed {
                continue;
            }
            let read = || {
                let crate_dir = fs::read_dir(dir.join("state"))
                    .ok()?
                    .flatten()
                    .next()?
                    .path();
                let graph = finished_session(&crate_dir)?.join("dep-graph.bin");
                let mut start = vec![0u8; 7 + 255 + 8];
                let n = fs::File::open(graph).ok()?.read(&mut start).ok()?;
                start.truncate(n);
                Some(start)
            };
            starts.push(read()?);
        }
        if failed {
            return None;
        }
        let (header, mark) = state_header(&starts[0])?;
        let (other_header, other_mark) = state_header(&starts[1])?;
        (header == other_header && mark != other_mark)
            .then(|| (header.to_vec(), <[u8; 8]>::try_from(mark).unwrap()))
    })();
    let _ = fs::remove_dir_all(&scratch);
    found
}

/// Before a compilation that has no saved state of rustc's to start from:
/// give it the state kept for the same command and output directory, from
/// this checkout or another. The compiler then recompiles what differs
/// instead of everything. Says whether state was given.
fn give_saved_state(job: &Job, store: &mut Store, recipe: &Digest) -> bool {
    let Some(directory) = &job.facts.incremental else {
        return false;
    };
    if !job.state_travels() {
        return false;
    }
    let kept = state_recipe(job, recipe);
    let Ok(candidates) = store.candidates(&kept) else {
        return false;
    };
    let mut lots: Vec<(&crate::cas::Candidate, Manifest)> = candidates
        .iter()
        .filter_map(|candidate| match Manifest::decode(&candidate.manifest) {
            Ok(manifest) if manifest.key(&kept) == candidate.key => Some((candidate, manifest)),
            _ => None,
        })
        .collect();
    // The newest first: there is more than one only while one is replaced.
    lots.sort_by_key(|(_, manifest)| std::cmp::Reverse(manifest.saved_at));
    for (candidate, manifest) in &lots {
        let Some(first) = manifest.saved.first() else {
            continue;
        };
        // One session directory, two levels into the directory rustc was
        // told to keep its state in, holds every file.
        let within = |file: &Output| {
            job.roots
                .join(file.root, &file.path)
                .and_then(|path| path.parent().map(Path::to_owned))
        };
        let Some(session) =
            within(first).filter(|s| s.parent().and_then(Path::parent) == Some(directory))
        else {
            continue;
        };
        if manifest
            .saved
            .iter()
            .any(|file| within(file).as_ref() != Some(&session))
        {
            continue;
        }
        // The compiler's own state for this crate is better than any other.
        if session
            .parent()
            .is_some_and(|crate_dir| finished_session(crate_dir).is_some())
        {
            return false;
        }
        if !store.place_saved(&manifest.saved, &job.roots) {
            continue;
        }
        // rustc takes a session whose lock file it can lock.
        let name = session.file_name().unwrap_or_default().to_string_lossy();
        let lock = name
            .rsplit_once('-')
            .map(|(stem, _)| session.with_file_name(format!("{stem}.lock")));
        let ready = (|| {
            fs::File::create(lock.as_ref()?).ok()?;
            if manifest.saved_in == whereabouts(&job.roots) {
                return Some(());
            }
            use std::os::unix::fs::FileExt;
            let (header, mark) = options_mark(job, store)?;
            let graph = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(session.join("dep-graph.bin"))
                .ok()?;
            let mut start = vec![0u8; header.len() + 8];
            graph.read_exact_at(&mut start, 0).ok()?;
            if start[..header.len()] != header[..] {
                // Saved by another version of the compiler.
                return None;
            }
            graph.write_all_at(&mark, header.len() as u64).ok()
        })();
        if ready.is_none() {
            let _ = fs::remove_dir_all(&session);
            if let Some(lock) = lock {
                let _ = fs::remove_file(lock);
            }
            continue;
        }
        store.touch(&candidate.key);
        store.count("state_given", 1);
        log(
            job.name(),
            "given the compiler's saved state from an earlier compilation",
        );
        return true;
    }
    false
}

/// Wait for another process that is running this same command on the same
/// source, so that its result can be reused. The lock is released when the
/// file is closed, however that happens.
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

/// The steps between finding no result and running the command: take this
/// command's place in line, and look again if someone else was ahead.
pub fn before_running(
    job: &Job,
    store: &mut Store,
    recipe: &Digest,
) -> (Option<fs::File>, Option<(Outcome, Manifest)>) {
    let source = job.facts.source.as_ref().and_then(|path| {
        store
            .digest_known(path, Stamp::of(&fs::metadata(path).ok()?))
            .ok()
    });
    let version = store.data_version();
    store.flush();
    let lock = queue(store, recipe, source);
    let waited = lock.as_ref().is_some_and(|(_, waited)| *waited);
    let lock = lock.map(|(file, _)| file);
    if waited || store.data_version() != version {
        match reuse(job, store, recipe, false) {
            Ok(Some(found)) => {
                store.count("coalesced", 1);
                return (lock, Some(found));
            }
            Ok(None) => (),
            Err(e) => log(job.name(), format_args!("lookup failed: {e:#}")),
        }
    }
    (lock, None)
}

/// How long after a compilation another of the same crate in the same
/// target directory counts as part of the same spell of editing.
const EDIT_WINDOW: u64 = 600;

/// How a compilation that has to run is run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Run {
    /// Watched and recorded.
    Recorded,
    /// One of a run of edits, compiled from the state the compiler saved
    /// here or was handed: neither watched nor recorded.
    Continued,
    /// One of a run of edits with nothing saved to start from, here or in
    /// the cache. Neither watched nor recorded; what the compiler saves is
    /// kept for other checkouts.
    Seeding,
}

/// Decide how this compilation is run, and hand the compiler kept state
/// if it has none of its own.
///
/// A crate compiled again within minutes of its last compilation in the
/// same target directory is being edited. rustc compiles it from the state
/// it saved, another edit follows, and what the compilation produces is
/// replaced before anything else could use it. Watching it would cost a
/// good part of the time it takes on a busy machine, and keeping its
/// output would fill the cache with states nobody returns to. The first
/// compilation after a pause is recorded as usual, which is how a checkout
/// that is built for months keeps contributing results.
///
/// `building` is set inside a build that is being recorded as a whole,
/// where nothing is left unwatched.
pub fn choose(job: &Job, store: &mut Store, recipe: &Digest, building: bool) -> Run {
    let Some(directory) = job.facts.incremental.as_ref() else {
        return Run::Recorded;
    };
    if job.scope != Scope::Unit {
        return Run::Recorded;
    }
    let own = has_saved_work(directory, job.name());
    let given = !own && saved_state_wanted() && give_saved_state(job, store, recipe);
    if building {
        return Run::Recorded;
    }
    let window = env::var("JATO_EDIT_WINDOW")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(EDIT_WINDOW);
    let mut hash = blake3::Hasher::new_derive_key("jato compiled here v1");
    hash.update(recipe);
    hash.update(directory.as_os_str().as_bytes());
    let recent = store
        .since_noted(hash.finalize().as_bytes())
        .is_some_and(|seconds| seconds < window);
    match (recent, own || given) {
        (false, _) => Run::Recorded,
        (true, true) => Run::Continued,
        (true, false) => Run::Seeding,
    }
}

/// Whether rustc has finished a session for this crate in the directory it
/// keeps incremental state in. Cargo makes the directory itself, so its
/// being there says nothing.
fn has_saved_work(saved: &Path, name: &str) -> bool {
    let prefix = format!("{name}-");
    let Ok(crates) = fs::read_dir(saved) else {
        return false;
    };
    crates
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        .any(|entry| {
            fs::read_dir(entry.path()).is_ok_and(|sessions| {
                sessions.flatten().any(|session| {
                    let name = session.file_name();
                    let name = name.to_string_lossy();
                    name.starts_with("s-")
                        && !name.ends_with("-working")
                        && !name.ends_with(".lock")
                })
            })
        })
}

/// Record a command that has run, and say what became of it.
pub fn finish(
    job: &Job,
    store: &mut Store,
    recipe: &Digest,
    ran: &Ran,
    report: &Report,
    parts: Option<&Parts>,
) -> Result<Manifest, String> {
    if !ran.success {
        return Err("it failed".into());
    }
    let begun = Instant::now();
    let recorded = record(job, store, recipe, ran, report, parts);
    match &recorded {
        Ok(manifest) => {
            store.count(job.counter("stored"), 1);
            log(
                job.name(),
                format_args!(
                    "stored: ran {} ms, then {} inputs and {} outputs recorded in {} ms",
                    ran.millis,
                    manifest.inputs.len(),
                    manifest.outputs.len(),
                    begun.elapsed().as_millis()
                ),
            );
        }
        Err(why) => {
            store.count(job.counter("uncacheable"), 1);
            log(job.name(), format_args!("not stored: {why}"));
        }
    }
    if job.scope == Scope::Build {
        store.note_unrecorded(recorded.as_ref().err().map(String::as_str));
    }
    recorded
}

/// Note a wait for a place among the running compilers, if there was one
/// worth the name.
pub fn waited(job: &Job, store: &mut Store, time: std::time::Duration) {
    let millis = time.as_millis() as u64;
    if millis >= 20 {
        store.count("queued", 1);
        store.count("queued_ms", millis);
        log(
            job.name(),
            format_args!("waited {millis} ms for other builds' compilers"),
        );
    }
}

/// A compiler command that only this process is watching.
fn wrap_alone(program: &OsStr, args: &[OsString]) -> Result<i32> {
    alone(
        Job::unit(Request::current(program, args)?, None),
        program,
        args,
    )
}

/// Look up, run and record a command that nothing else is watching.
fn alone(job: Job, program: &OsStr, args: &[OsString]) -> Result<i32> {
    let mut store = match Store::open() {
        Ok(store) => store,
        Err(e) => {
            log(job.name(), format_args!("no cache: {e:#}"));
            return passthrough(original(program, args));
        }
    };
    let recipe = job.recipe();
    let print = |outcome: Outcome| -> Result<i32> {
        io::stdout().write_all(&outcome.stdout)?;
        io::stderr().write_all(&outcome.stderr)?;
        Ok(outcome.code)
    };
    match reuse(&job, &mut store, &recipe, false) {
        Ok(Some((outcome, _))) => return print(outcome),
        Ok(None) => (),
        Err(e) => log(job.name(), format_args!("lookup failed: {e:#}")),
    }
    if !observe::available() {
        // Nothing can be recorded, but the command must still run as given.
        store.count("unobserved", 1);
        drop(store);
        return passthrough(original(program, args));
    }
    let lock = match before_running(&job, &mut store, &recipe) {
        (_, Some((outcome, _))) => return print(outcome),
        (lock, None) => lock,
    };
    let run = choose(&job, &mut store, &recipe, false);
    // The compiler will run. With other builds going on, wait for a
    // processor to run it on.
    let place = if job.tool || job.facts.compiles() {
        store.flush();
        let place = crate::jobs::take(&store.dir);
        waited(&job, &mut store, place.waited);
        Some(place)
    } else {
        None
    };
    let argv = job.argv();
    match run {
        Run::Continued => {
            log(
                job.name(),
                "one of a run of edits: left to rustc, unrecorded",
            );
            store.count("continued", 1);
            drop(lock);
            drop(store);
            let mut cmd = original(&argv[0], &argv[1..]);
            if let Some(dir) = job.held()
                && hold(&mut cmd, dir).is_err()
            {
                // Without the directory only Cargo's own command line
                // means anything.
                cmd = original(program, args);
            }
            if let Some(place) = place {
                cmd.env(crate::jobs::HELD, "1");
                place.keep_through_exec();
            }
            return passthrough(cmd);
        }
        Run::Seeding => {
            log(
                job.name(),
                "being edited: compiled with saved state from now on, unrecorded",
            );
            store.count("continued", 1);
            store.flush();
            let since = std::time::SystemTime::now();
            let mut cmd = original(&argv[0], &argv[1..]);
            if let Some(dir) = job.held()
                && hold(&mut cmd, dir).is_err()
            {
                drop(lock);
                drop(store);
                return passthrough(original(program, args));
            }
            let status = cmd
                .env(crate::jobs::HELD, "1")
                .status()
                .context("starting the compiler")?;
            drop(place);
            if status.success() {
                keep_state(&job, &mut store, &recipe, Left::Since(since));
            }
            drop(lock);
            return Ok(exit_code(status));
        }
        Run::Recorded => (),
    }
    store.count(job.counter("miss"), 1);
    let (ran, report) = match run_piped(&argv, Some((&job, &store)), job.held()) {
        Ok(ran) => ran,
        Err(e) => {
            // The command may not have started at all. Run it plainly so
            // its own failure, if any, is what Cargo sees.
            log(job.name(), format_args!("not observed: {e:#}"));
            drop(lock);
            drop(store);
            return passthrough(original(program, args));
        }
    };
    drop(place);
    if finish(&job, &mut store, &recipe, &ran, &report, None).is_ok() {
        keep_state(&job, &mut store, &recipe, Left::Seen(&report.state));
    }
    drop(lock);
    Ok(ran.code)
}

pub fn wrap(program: &OsStr, args: &[OsString]) -> Result<i32> {
    started();
    if env::var_os("JATO_DISABLE").is_some_and(|v| v == "1") {
        return passthrough(original(program, args));
    }
    // Only a compiler reading its source from nothing can be answered
    // without knowing what it was sent.
    if args.iter().any(|a| a == "-")
        && fs::read_link("/proc/self/fd/0").ok().as_deref() != Some(Path::new("/dev/null"))
    {
        return passthrough(original(program, args));
    }
    if let Some(session) = env::var_os("JATO_SESSION")
        && let Some(code) = crate::session::ask(&session, program, args, false)?
    {
        return Ok(code);
    }
    wrap_alone(program, args)
}

/// Run the compiler or assembler `program`, found for the name this
/// process was started under, or put back what it made before. That is
/// for build scripts: inside a build Jato supervises the supervisor is
/// asked, and in any other this process watches the program itself.
/// Started by anything else, such as a program or a test Cargo runs, or
/// where it may be sent something to read, the program is simply run.
pub fn stand_in(program: &Path, args: &[OsString]) -> Result<i32> {
    started();
    let reads_nothing =
        fs::read_link("/proc/self/fd/0").ok().as_deref() == Some(Path::new("/dev/null"));
    let program = program.as_os_str();
    // What Cargo tells a build script and nothing else it starts.
    let script = ["OUT_DIR", "TARGET", "HOST", "OPT_LEVEL", "NUM_JOBS"]
        .iter()
        .all(|name| env::var_os(name).is_some());
    // rustc links through the C compiler, naming a directory of its own
    // that is new every time. That run is rustc's work, never the same
    // command twice.
    let parent = fs::read_to_string(format!(
        "/proc/{}/comm",
        std::os::unix::process::parent_id()
    ));
    let linking = parent.is_ok_and(|name| name.trim_end() == "rustc");
    if !script || linking || !reads_nothing || env::var_os("JATO_DISABLE").is_some_and(|v| v == "1")
    {
        return passthrough(original(program, args));
    }
    if let Some(session) = env::var_os("JATO_SESSION")
        && let Some(code) = crate::session::ask(&session, program, args, true)?
    {
        return Ok(code);
    }
    // Nothing supervises this build: Cargo is carrying out a few steps in
    // a target that is already built, or the build as a whole is not
    // being recorded.
    alone(
        Job::tool(Request::current(program, args)?, None),
        program,
        args,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_names_the_variables_it_watches_in_both_layouts() {
        // One package-relative file and no variables, as Cargo 1.100 writes.
        let new = b"\x01\0\0\0\xff\x01\x01\0\0\0\0\x0a\0\0\0src/lib.rs\0\0\0\0\0";
        assert_eq!(cargo_env_deps(new), Some(vec![]));
        let mut with = b"\x01\0\0\0\xff\x01\x01\0\0\0\0\x0a\0\0\0src/lib.rs\0".to_vec();
        with.extend_from_slice(b"\x02\0\0\0\x03\0\0\0ONE\x01\x01\0\0\0x\x03\0\0\0TWO\0");
        assert_eq!(
            cargo_env_deps(&with),
            Some(vec![b"ONE".to_vec(), b"TWO".to_vec()])
        );
        // The same without the marker and checksum, as before.
        let old = b"\x01\0\0\0\0\x0a\0\0\0src/lib.rs\x01\0\0\0\x03\0\0\0ONE\0";
        assert_eq!(cargo_env_deps(old), Some(vec![b"ONE".to_vec()]));
        assert_eq!(cargo_env_deps(&with[..with.len() - 1]), None);
        assert_eq!(cargo_env_deps(b"not this at all"), None);
    }

    #[test]
    fn a_build_script_names_the_variables_it_watches() {
        let printed = b"cargo:rerun-if-changed=build.rs\ncargo:rerun-if-env-changed=CC\ncargo::rerun-if-env-changed=ZLIB \n";
        assert_eq!(
            script_env_deps(printed),
            vec![b"CC".to_vec(), b"ZLIB".to_vec()]
        );
    }
}
