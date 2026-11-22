//! A whole Cargo command as one result, and the service its compiler
//! wrappers talk to while it runs.
//!
//! `jato build` first asks whether this exact build has been done before:
//! the same command, with every file Cargo and the compilers examined
//! unchanged. If so the target directory is filled in directly and Cargo
//! does not run. Otherwise Cargo runs under one observer. Each compiler
//! wrapper it starts asks this process, over a socket, to look its command
//! up or to record it, so the wrappers stay small and the observer can tell
//! one compiler's file accesses from another's.
use crate::cas::Store;
use crate::command::{self, Job, Outcome, Parts, Ran, Request, log};
use crate::manifest::Env;
use crate::observe::{self, Scopes};
use crate::roots::{Root, Roots};
use crate::wire::{self, Reader, Writer};
use anyhow::{Context, Result, bail};
use std::ffi::{OsStr, OsString};
use std::io::{self, IsTerminal, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const HIT: u8 = 0;
const RUN: u8 = 1;
const DECLINE: u8 = 2;
const EXEC: u8 = 3;

fn os(bytes: &[u8]) -> OsString {
    OsString::from_vec(bytes.to_vec())
}

fn address(name: &OsStr) -> io::Result<SocketAddr> {
    SocketAddr::from_abstract_name(name.as_bytes())
}

// ---- the wrapper's side ---------------------------------------------------

/// Hand a compiler command to the build's supervisor. None if there is no
/// supervisor to ask, in which case the caller handles the command alone.
pub fn ask(session: &OsStr, program: &OsStr, args: &[OsString]) -> Result<Option<i32>> {
    let Ok(mut stream) = address(session).and_then(|addr| UnixStream::connect_addr(&addr)) else {
        return Ok(None);
    };
    let mut w = Writer(Vec::new());
    w.number(std::process::id().into());
    w.bytes(std::env::current_dir()?.as_os_str().as_bytes());
    w.bytes(program.as_bytes());
    w.number(args.len() as u64);
    for arg in args {
        w.bytes(arg.as_bytes());
    }
    let vars: Vec<_> = std::env::vars_os().collect();
    w.number(vars.len() as u64);
    for (name, value) in &vars {
        w.bytes(name.as_bytes());
        w.bytes(value.as_bytes());
    }
    if wire::send(&mut stream, &w.0).is_err() {
        return Ok(None);
    }
    let Ok(reply) = wire::receive(&mut stream) else {
        return Ok(None);
    };
    let mut r = Reader(&reply);
    match r.byte()? {
        HIT => {
            let code = r.number()? as i32;
            io::stdout().write_all(r.bytes()?)?;
            io::stderr().write_all(r.bytes()?)?;
            Ok(Some(code))
        }
        RUN => {
            let mut argv = Vec::new();
            for _ in 0..r.count()? {
                argv.push(os(r.bytes()?));
            }
            // The supervisor is already watching this process's children.
            let (ran, _) = command::run_piped(&argv, None)?;
            let mut w = Writer(Vec::new());
            w.number(ran.code as u32 as u64);
            w.0.push(ran.success.into());
            w.optional(ran.stdout.as_deref());
            w.optional(ran.stderr.as_deref());
            w.number(ran.millis);
            // The result is stored before Cargo is told the command is
            // done, so whatever Cargo starts next can find it.
            if wire::send(&mut stream, &w.0).is_ok() {
                let _ = wire::receive(&mut stream);
            }
            Ok(Some(ran.code))
        }
        EXEC => {
            // Run it and report nothing.
            let mut argv = Vec::new();
            for _ in 0..r.count()? {
                argv.push(os(r.bytes()?));
            }
            drop(stream);
            let mut cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..]);
            command::passthrough(cmd).map(Some)
        }
        _ => Ok(None),
    }
}

// ---- the supervisor's side ------------------------------------------------

struct Server {
    scopes: Scopes,
    roots: Roots,
    parts: Mutex<Parts>,
    stores: Mutex<Vec<Store>>,
    stop: AtomicBool,
}

impl Server {
    fn store(&self) -> Result<Store> {
        let spare = self.stores.lock().unwrap_or_else(|e| e.into_inner()).pop();
        match spare {
            Some(store) => Ok(store),
            None => Store::open(),
        }
    }

    fn unusable(&self, why: &str) {
        let mut parts = self.parts.lock().unwrap_or_else(|e| e.into_inner());
        parts.unusable.get_or_insert_with(|| why.to_owned());
    }

    fn used(&self, job: &Job, manifest: &crate::manifest::Manifest) {
        let mut parts = self.parts.lock().unwrap_or_else(|e| e.into_inner());
        parts.add(manifest, &job.roots, &self.roots);
    }

    /// Serve one compiler wrapper from its request to its last message.
    fn serve(&self, mut stream: UnixStream) -> Result<()> {
        let message = wire::receive(&mut stream)?;
        let mut r = Reader(&message);
        let pid = r.number()? as u32;
        let cwd = PathBuf::from(os(r.bytes()?));
        let program = os(r.bytes()?);
        let mut args = Vec::new();
        for _ in 0..r.count()? {
            args.push(os(r.bytes()?));
        }
        let mut env = Env::new();
        for _ in 0..r.count()? {
            let name = os(r.bytes()?);
            env.insert(name, os(r.bytes()?));
        }
        let job = Job::unit(
            Request {
                program,
                args,
                cwd,
                env,
            },
            self.roots.path(crate::roots::TARGET),
        );
        let mut store = match self.store() {
            Ok(store) => store,
            Err(e) => {
                log(job.name(), format_args!("no cache: {e:#}"));
                self.unusable("a compiler ran without the cache");
                return wire::send(&mut stream, &[DECLINE]).map_err(Into::into);
            }
        };
        let recipe = job.recipe();
        let reply = |stream: &mut UnixStream, outcome: &Outcome| {
            let mut w = Writer(vec![HIT]);
            w.number(outcome.code as u32 as u64);
            w.bytes(&outcome.stdout);
            w.bytes(&outcome.stderr);
            wire::send(stream, &w.0)
        };
        let mut found = match command::reuse(&job, &mut store, &recipe, false) {
            Ok(found) => found,
            Err(e) => {
                log(job.name(), format_args!("lookup failed: {e:#}"));
                None
            }
        };
        let mut lock = None;
        if found.is_none() {
            (lock, found) = command::before_running(&job, &mut store, &recipe);
        }
        if let Some((outcome, manifest)) = found {
            self.used(&job, &manifest);
            store.flush();
            self.stores
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(store);
            return reply(&mut stream, &outcome).map_err(Into::into);
        }
        let argv = job.argv();
        let command = |tag: u8| {
            let mut w = Writer(vec![tag]);
            w.number(argv.len() as u64);
            for arg in &argv {
                w.bytes(arg.as_bytes());
            }
            w
        };
        if command::continues(&job, &mut store, &recipe) {
            log(
                job.name(),
                "one of a run of edits: left to rustc, unrecorded",
            );
            store.count("continued", 1);
            // Nothing says what this compilation depended on.
            self.unusable("a compilation was left unrecorded");
            store.flush();
            self.stores
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(store);
            return wire::send(&mut stream, &command(EXEC).0).map_err(Into::into);
        }
        store.count(job.counter("miss"), 1);
        self.scopes.begin(pid, job.observer(&store));
        let w = command(RUN);
        let done = wire::send(&mut stream, &w.0).and_then(|()| wire::receive(&mut stream));
        let report = self.scopes.end(pid).unwrap_or_default();
        let Ok(done) = done else {
            // The wrapper went away mid-compilation: the build was stopped.
            self.unusable("a compiler was interrupted");
            return Ok(());
        };
        let mut r = Reader(&done);
        let ran = Ran {
            code: r.number()? as i32,
            success: r.byte()? != 0,
            stdout: r.optional()?.map(<[u8]>::to_vec),
            stderr: r.optional()?.map(<[u8]>::to_vec),
            millis: r.number()?,
        };
        match command::finish(&job, &mut store, &recipe, &ran, &report, None) {
            Ok(manifest) => self.used(&job, &manifest),
            // A failed compilation fails the build. One that worked but
            // cannot be reused leaves nothing to say what it depended on.
            Err(why) if ran.success => self.unusable(&format!("{}: {why}", job.name())),
            Err(_) => (),
        }
        drop(lock);
        store.flush();
        self.stores
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(store);
        let _ = wire::send(&mut stream, &[0]);
        Ok(())
    }
}

/// The nearest directory at or above `from` whose manifest declares a
/// workspace, or failing that the nearest with a manifest at all.
fn workspace_root(from: &Path) -> Option<PathBuf> {
    let mut nearest = None;
    for dir in from.ancestors() {
        let manifest = dir.join("Cargo.toml");
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        if text
            .lines()
            .any(|line| line.trim_start().starts_with("[workspace"))
        {
            return Some(dir.to_owned());
        }
        nearest.get_or_insert_with(|| dir.to_owned());
    }
    nearest
}

/// Where this Cargo command works: the workspace, and the target directory
/// as far as the command line and environment say. A target directory set
/// only in a configuration file is not found; results are then kept at the
/// paths they were written to.
fn locate(request: &Request) -> (Roots, Vec<usize>) {
    let args = &request.args;
    let end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let mut target = None;
    let mut manifest = None;
    let mut skipped = Vec::new();
    let mut i = 0;
    while i < end {
        let arg = args[i].as_bytes();
        for (flag, slot) in [
            (&b"--target-dir"[..], &mut target),
            (&b"--manifest-path"[..], &mut manifest),
        ] {
            if arg == flag && i + 1 < end {
                *slot = Some((PathBuf::from(&args[i + 1]), vec![i, i + 1]));
            } else if let Some(value) = arg.strip_prefix(flag).and_then(|v| v.strip_prefix(b"=")) {
                *slot = Some((PathBuf::from(OsStr::from_bytes(value)), vec![i]));
            }
        }
        i += 1;
    }
    let start = match &manifest {
        Some((path, _)) => request.cwd.join(path).parent().map(Path::to_owned),
        None => Some(request.cwd.clone()),
    };
    let work = start.as_deref().and_then(workspace_root);
    let target = match target {
        // Where the target directory is does not change what is built.
        Some((path, at)) => {
            skipped = at;
            Some(request.cwd.join(path))
        }
        None => ["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"]
            .iter()
            .find_map(|name| request.env.get(OsStr::new(name)))
            .map(|path| request.cwd.join(path))
            .or_else(|| work.as_ref().map(|w| w.join("target"))),
    };
    let roots = Roots {
        work: work.as_deref().and_then(Root::new),
        target: target.as_deref().and_then(Root::new),
    };
    (roots, skipped)
}

/// A terminal pair for a child's diagnostics, so Cargo behaves as it does
/// when run directly: colours, progress and a width for the compiler.
fn terminal() -> Option<(OwnedFd, OwnedFd)> {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(2, libc::TIOCGWINSZ, &raw mut size) } != 0 {
        return None;
    }
    let (mut master, mut slave) = (0, 0);
    let made = unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &raw const size,
        )
    };
    if made != 0 {
        return None;
    }
    unsafe {
        for fd in [master, slave] {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        // Pass bytes through as they are: no newline translation on the
        // way to the real terminal, which does its own.
        let mut mode: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(slave, &raw mut mode) == 0 {
            mode.c_oflag &= !libc::OPOST;
            libc::tcsetattr(slave, libc::TCSANOW, &raw const mode);
        }
        Some((OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)))
    }
}

/// What Cargo would print for a build in which nothing had to be done,
/// taken from what it printed when everything was. Lines that report
/// progress go; diagnostics and the summary stay, with the time it took
/// this time.
fn replayed(recorded: &[u8], elapsed: f64, quiet: bool) -> Vec<u8> {
    const PROGRESS: &[&str] = &[
        "Compiling",
        "Checking",
        "Building",
        "Running",
        "Fresh",
        "Documenting",
        "Blocking",
        "Locking",
        "Downloading",
        "Downloaded",
        "Adding",
        "Updating",
        "Dirty",
    ];
    let mut out = Vec::with_capacity(recorded.len());
    for line in recorded.split_inclusive(|b| *b == b'\n') {
        // What a terminal would show: the text after the last return to
        // the start of the line, without colour changes.
        let shown = line.rsplit(|b| *b == b'\r').find(|part| part.len() > 1);
        let shown = shown.unwrap_or(line);
        let mut plain = Vec::with_capacity(shown.len());
        let mut bytes = shown.iter().copied().peekable();
        while let Some(b) = bytes.next() {
            if b == 0x1b && bytes.peek() == Some(&b'[') {
                for c in bytes.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                plain.push(b);
            }
        }
        let text = String::from_utf8_lossy(&plain);
        let word = text.trim_start().split(' ').next().unwrap_or_default();
        if PROGRESS.contains(&word) && text.starts_with(' ') {
            continue;
        }
        if word == "Finished" && text.starts_with(' ') {
            if !quiet && let Some(at) = memchr::memmem::rfind(shown, b" in ") {
                out.extend_from_slice(&shown[..at]);
                out.extend_from_slice(format!(" in {elapsed:.2}s\n").as_bytes());
            }
            continue;
        }
        out.extend_from_slice(shown);
    }
    out
}

/// A Cargo command made ready to be looked up.
struct Prepared {
    job: Job,
    store: Store,
    recipe: crate::scan::Digest,
}

fn prepare(cmd: &Command, name: &str) -> Result<Option<Prepared>> {
    command::started();
    if std::env::var_os("JATO_DISABLE").is_some_and(|v| v == "1")
        || std::env::var_os("JATO_WHOLE_BUILDS").is_some_and(|v| v == "0")
        || !observe::available()
    {
        return Ok(None);
    }
    let mut env: Env = std::env::vars_os().collect();
    for (name, value) in cmd.get_envs() {
        match value {
            Some(value) => env.insert(name.to_owned(), value.to_owned()),
            None => env.remove(name),
        };
    }
    let request = Request {
        program: cmd.get_program().to_owned(),
        args: cmd.get_args().map(OsStr::to_owned).collect(),
        cwd: std::env::current_dir()?,
        env,
    };
    let (roots, skipped) = locate(&request);
    // Diagnostics are laid out for the terminal they are shown on.
    let terminal_out = io::stderr().is_terminal();
    let mut salt = vec![u8::from(terminal_out), u8::from(io::stdout().is_terminal())];
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    if terminal_out && unsafe { libc::ioctl(2, libc::TIOCGWINSZ, &raw mut size) } == 0 {
        salt.extend_from_slice(&size.ws_col.to_le_bytes());
    }
    // Whether the command stops before running what it built is a property
    // of the build, wherever the option stands. How much Cargo says about
    // its progress is not: none of that is repeated for a restored build.
    let end = request
        .args
        .iter()
        .position(|a| a == "--")
        .unwrap_or(request.args.len());
    let mut skipped = skipped;
    for (i, arg) in request.args[..end].iter().enumerate() {
        if arg == "--no-run" {
            salt.push(b'n');
            skipped.push(i);
        } else if arg == "-q" || arg == "--quiet" {
            skipped.push(i);
        }
    }
    let mut job = Job::build(request, name, roots, salt);
    job.facts.skipped = skipped;
    let store = match Store::open() {
        Ok(store) => store,
        Err(e) => {
            log(name, format_args!("no cache: {e:#}"));
            return Ok(None);
        }
    };
    let recipe = job.recipe();
    Ok(Some(Prepared { job, store, recipe }))
}

/// Fill in what `cmd` would build if that exact build has been recorded,
/// without printing anything. Says whether it has.
pub fn restore(cmd: &Command, name: &str) -> bool {
    let Ok(Some(mut prepared)) = prepare(cmd, name) else {
        return false;
    };
    let found = command::reuse(&prepared.job, &mut prepared.store, &prepared.recipe, true);
    matches!(found, Ok(Some(_)))
}

/// Run a Cargo command that only builds, or fill in what it would build.
pub fn build(mut cmd: Command, name: &str) -> Result<i32> {
    let begun = Instant::now();
    let Some(Prepared {
        job,
        mut store,
        recipe,
    }) = prepare(&cmd, name)?
    else {
        return command::passthrough(cmd);
    };
    match command::reuse(&job, &mut store, &recipe, true) {
        Ok(Some((outcome, _))) => {
            // Cargo reports each artifact it did not have to rebuild.
            let fresh = String::from_utf8_lossy(&outcome.stdout)
                .replace("\"fresh\":false", "\"fresh\":true");
            io::stdout().write_all(fresh.as_bytes())?;
            let quiet = cmd.get_args().any(|a| a == "-q" || a == "--quiet");
            let elapsed = begun.elapsed().as_secs_f64();
            io::stderr().write_all(&replayed(&outcome.stderr, elapsed, quiet))?;
            return Ok(outcome.code);
        }
        Ok(None) => (),
        Err(e) => log(name, format_args!("lookup failed: {e:#}")),
    }
    // An edit in a target that is already built: Cargo will compile a few
    // crates and find the rest up to date. Each compiler it starts is
    // watched on its own. Watching Cargo check everything else as well
    // would add more to this build than its record could save later.
    if command::mostly_built(&job, &store, &recipe) {
        log(
            name,
            "the target is already built: leaving this build to Cargo",
        );
        store.count("build_left_to_cargo", 1);
        drop(store);
        return command::passthrough(cmd);
    }
    let terminal_out = io::stderr().is_terminal();
    store.count(job.counter("miss"), 1);

    // No other process can use this name, and it needs no cleaning up.
    let socket = format!(
        "jato-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let listener = UnixListener::bind_addr(&address(OsStr::new(&socket))?)?;
    cmd.env("JATO_SESSION", &socket);
    cmd.stdout(Stdio::piped());
    let pty = terminal_out.then(terminal).flatten();
    let master = match pty {
        Some((master, slave)) => {
            cmd.stderr(Stdio::from(slave));
            Some(master)
        }
        None => {
            cmd.stderr(Stdio::piped());
            None
        }
    };
    let config = observe::Config {
        ignore: vec![store.dir.clone()],
        unlisted: Vec::new(),
    };
    let (mut child, session) = match observe::spawn(&mut cmd, config) {
        Ok(started) => started,
        Err(e) => {
            log(name, format_args!("not observed: {e:#}"));
            cmd.env_remove("JATO_SESSION")
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
            return command::passthrough(cmd);
        }
    };
    // The command holds the other end of the terminal pair. Keeping a copy
    // here would hide its closing.
    let child_err = child.stderr.take();
    drop(cmd);
    let server = Arc::new(Server {
        scopes: session.scopes(),
        roots: job.roots.clone(),
        parts: Mutex::new(Parts::default()),
        stores: Mutex::new(Vec::new()),
        stop: AtomicBool::new(false),
    });
    let accepting = server.clone();
    let accept = std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if accepting.stop.load(Ordering::Acquire) {
                break;
            }
            let server = accepting.clone();
            std::thread::spawn(move || {
                if let Err(e) = server.serve(stream) {
                    server.unusable(&format!("a compiler's request failed: {e:#}"));
                }
            });
        }
    });
    // Stopping this process alone would leave the compilers waiting for
    // answers to their system calls. Pass the signal on and wait.
    let pid = child.id() as i32;
    let _ = ctrlc::set_handler(move || unsafe {
        libc::kill(pid, libc::SIGTERM);
    });
    let out = child.stdout.take().unwrap();
    let copy_out = std::thread::spawn(move || command::tee(out, io::stdout()));
    let (stderr, streamed) = match (master, child_err) {
        (Some(master), _) => command::tee(std::fs::File::from(master), io::stderr()),
        (None, Some(err)) => command::tee(err, io::stderr()),
        (None, None) => bail!("the command's output could not be read"),
    };
    let (stdout, streamed_out) = copy_out
        .join()
        .unwrap_or((None, Err(io::Error::other("output thread failed"))));
    if streamed.and(streamed_out).is_err() {
        let _ = child.kill();
    }
    let status = child.wait().context("waiting for Cargo")?;
    server.stop.store(true, Ordering::Release);
    let _ = UnixStream::connect_addr(&address(OsStr::new(&socket))?);
    let _ = accept.join();
    let report = session.finish();
    let ran = Ran {
        code: command::exit_code(status),
        success: status.success(),
        stdout,
        stderr,
        millis: begun.elapsed().as_millis() as u64,
    };
    let parts = server.parts.lock().unwrap_or_else(|e| e.into_inner());
    log(
        name,
        format_args!("ran, with {} file accesses of its own", report.paths.len()),
    );
    let _ = command::finish(&job, &mut store, &recipe, &ran, &report, Some(&parts));
    store.flush();
    Ok(ran.code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_restored_build_reads_like_one_with_nothing_to_do() {
        let recorded = [
            "\x1b[1m\x1b[92m   Compiling\x1b[0m leaf v0.1.0 (/w/leaf)\n",
            "warning: unused variable: `x`\n",
            " --> leaf/src/lib.rs:1:5\n",
            "\n",
            "warning: `leaf` (lib) generated 1 warning\n",
            "    Building [=====>   ] 1/2: app\r\x1b[K   Compiling app v0.1.0 (/w)\n",
            "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 12.50s\n",
        ]
        .concat();
        let text = String::from_utf8(replayed(recorded.as_bytes(), 0.031, false)).unwrap();
        let expected = [
            "warning: unused variable: `x`\n",
            " --> leaf/src/lib.rs:1:5\n",
            "\n",
            "warning: `leaf` (lib) generated 1 warning\n",
            "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.03s\n",
        ];
        assert_eq!(text, expected.concat());
        let quiet = String::from_utf8(replayed(recorded.as_bytes(), 0.031, true)).unwrap();
        assert_eq!(quiet, expected[..expected.len() - 1].concat());
        // Text that merely starts with such a word is a diagnostic.
        assert_eq!(
            replayed(b"Running out of ideas\n", 1.0, false),
            b"Running out of ideas\n"
        );
    }

    #[test]
    fn the_target_directory_comes_from_the_command_or_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir_all(root.join("crates/a")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers=['crates/*']\n",
        )
        .unwrap();
        std::fs::write(root.join("crates/a/Cargo.toml"), "[package]\nname='a'\n").unwrap();
        let request = |args: &[&str], env: &[(&str, &str)]| Request {
            program: "cargo".into(),
            args: args.iter().map(OsString::from).collect(),
            cwd: root.join("crates/a"),
            env: env
                .iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v)))
                .collect(),
        };
        let (roots, skipped) = locate(&request(&["build"], &[]));
        assert_eq!(roots.path(crate::roots::WORK), Some(root.as_path()));
        assert_eq!(
            roots.path(crate::roots::TARGET),
            Some(root.join("target").as_path())
        );
        assert!(skipped.is_empty());
        let (roots, _) = locate(&request(
            &["build"],
            &[("CARGO_TARGET_DIR", "/elsewhere/t")],
        ));
        assert_eq!(
            roots.path(crate::roots::TARGET),
            Some(Path::new("/elsewhere/t"))
        );
        let (roots, skipped) = locate(&request(
            &["build", "--target-dir", "out", "--", "--target-dir", "x"],
            &[("CARGO_TARGET_DIR", "/elsewhere/t")],
        ));
        assert_eq!(
            roots.path(crate::roots::TARGET),
            Some(root.join("crates/a/out").as_path())
        );
        assert_eq!(skipped, [1, 2]);
        let (roots, skipped) = locate(&request(&["build", "--target-dir=/abs/out"], &[]));
        assert_eq!(
            roots.path(crate::roots::TARGET),
            Some(Path::new("/abs/out"))
        );
        assert_eq!(skipped, [1]);
    }
}
