//! Run a command, or reuse the result of an earlier run.
//!
//! Nothing here knows what a command computes. A result is the files the
//! command wrote and what it printed, and it may stand in for a later run of
//! the same command line when everything the earlier run looked at is as it
//! was. What it looked at is observed, not inferred from its arguments.
use crate::cas::{Staged, Store};
use crate::facts::{Facts, keyed};
use crate::manifest::{Env, Here, Input, Manifest, Output, Place, State, path_dirs, which};
use crate::observe::{self, Kind, Report, Stamp};
use crate::roots::{self, Roots};
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
    *STARTED.get_or_init(Instant::now)
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
}

impl Request {
    pub fn current(program: &OsStr, args: &[OsString]) -> Result<Self> {
        Ok(Self {
            program: program.to_owned(),
            args: args.to_vec(),
            cwd: env::current_dir()?,
            env: env::vars_os().collect(),
        })
    }
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
    path: Vec<PathBuf>,
    /// Further facts that tell this command from others like it.
    pub salt: Vec<u8>,
}

impl Job {
    /// A compiler command as Cargo runs it.
    pub fn unit(request: Request) -> Self {
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
        let roots = Roots::detect(&request.cwd, hint.as_deref());
        let remap = facts.compiles()
            && !facts.keeps_real_paths
            && cfg!(target_os = "linux")
            && env::var_os("JATO_REMAP").is_none_or(|v| v != "0")
            && (roots.work.is_some() || roots.target.is_some());
        Self {
            path: path_dirs(&request.cwd, &request.env),
            request,
            facts,
            roots,
            scope: Scope::Unit,
            remap,
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
        field(&mut hash, &self.salt);
        *hash.finalize().as_bytes()
    }

    /// The command line to run, which for a compilation names aliases for
    /// the roots.
    pub fn argv(&self) -> Vec<OsString> {
        let args = &self.request.args;
        let mut argv = vec![self.request.program.clone()];
        // Another wrapper is given the compiler's path first. Options start
        // after it.
        let options = args
            .iter()
            .position(|a| a.as_bytes().starts_with(b"-"))
            .unwrap_or(0);
        argv.extend_from_slice(&args[..options]);
        if self.remap {
            // Later options win, so the user's own remappings still apply,
            // and the target directory wins over a workspace containing it.
            for (root, alias) in [(roots::WORK, WORK_ALIAS), (roots::TARGET, TARGET_ALIAS)] {
                if let Some(path) = self.roots.path(root) {
                    let mut arg = OsString::from("--remap-path-prefix=");
                    arg.push(path);
                    arg.push("=");
                    arg.push(alias);
                    argv.push(arg);
                }
            }
        }
        argv.extend_from_slice(&args[options..]);
        argv
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
pub fn run_piped(argv: &[OsString], observed: Option<(&Job, &Store)>) -> Result<(Ran, Report)> {
    let begun = Instant::now();
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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
    // What this command made itself, here or in one of its parts, cannot
    // also be something it depended on.
    let mut made: HashSet<Place> = parts
        .map(|parts| parts.outputs.keys().cloned().collect())
        .unwrap_or_default();
    // The names found in each listed directory. Those that turn out to be
    // this command's own outputs are taken out before they are recorded.
    let mut listings: HashMap<Place, Vec<OsString>> = HashMap::new();
    let mut watched: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut paths: Vec<_> = report.paths.iter().collect();
    paths.sort_by_key(|(path, _)| *path);
    for (path, access) in paths {
        if job.state(path) {
            continue;
        }
        let (root, rel) = roots.split(path);
        let now = fs::symlink_metadata(path);
        if access.written {
            // Cargo compares what it is about to write with what is there,
            // and rewrites. What was there does not change what it writes.
            let stale = artifacts.contains(access.origin.as_ref().unwrap_or(path))
                || (job.scope == Scope::Build && root == roots::TARGET);
            if access.read && !stale {
                return Err(format!("it changed {}, which it had read", path.display()));
            }
            if access.updated && !stale && now.as_ref().is_ok_and(|m| !m.is_dir()) {
                return Err(format!("it wrote into {} as it found it", path.display()));
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
        for (name, found) in &parts.which {
            if !manifest.which.iter().any(|(n, _)| n == name) {
                manifest.which.push((name.clone(), found.clone()));
            }
        }
        manifest.pins.extend(parts.pins.iter().cloned());
        manifest.removed.extend(parts.removed.iter().cloned());
    }
    if job.scope == Scope::Build {
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
        let written = |name: &OsStr| {
            outputs.iter().any(|(root, rel, _)| {
                *root == place.0 && rel.parent() == Some(&place.1) && rel.file_name() == Some(name)
            })
        };
        match (&input.state, listings.remove(&place)) {
            (State::Dir(None), Some(mut names)) => {
                names.retain(|name| !written(name));
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
            if let Some((_, path)) = manifest.pins.first() {
                store.count("pinned", 1);
                log(
                    job.name(),
                    format_args!(
                        "its output names {}, so it is only reused there",
                        path.display()
                    ),
                );
            }
            Ok(manifest)
        }
        Ok(false) => Err("it is larger than half the cache".into()),
        Err(e) => Err(format!("{e:#}")),
    }
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
    let recorded = record(job, store, recipe, ran, report, parts);
    match &recorded {
        Ok(_) => {
            store.count(job.counter("stored"), 1);
            log(job.name(), "stored");
        }
        Err(why) => {
            store.count(job.counter("uncacheable"), 1);
            log(job.name(), format_args!("not stored: {why}"));
        }
    }
    recorded
}

/// A compiler command that only this process is watching.
fn wrap_alone(program: &OsStr, args: &[OsString]) -> Result<i32> {
    let job = Job::unit(Request::current(program, args)?);
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
    store.count(job.counter("miss"), 1);
    let (ran, report) = match run_piped(&job.argv(), Some((&job, &store))) {
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
    let _ = finish(&job, &mut store, &recipe, &ran, &report, None);
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
        && let Some(code) = crate::session::ask(&session, program, args)?
    {
        return Ok(code);
    }
    wrap_alone(program, args)
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
