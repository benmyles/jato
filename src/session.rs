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
use crate::cas::{SharedStamps, Store};
use crate::command::{self, Job, LookupCache, Outcome, Parts, Ran, Request, Run, log};
use crate::manifest::Env;
use crate::observe::{self, Report, Scopes};
use crate::roots::{Root, Roots};
use crate::wire::{self, Reader, Writer};
use anyhow::{Context, Result, bail};
use std::ffi::{OsStr, OsString};
use std::io::{self, IsTerminal, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

const REQ_COMPILER: u8 = 0;

const HIT: u8 = 0;
const RUN: u8 = 1;
const DECLINE: u8 = 2;
const CONTINUE: u8 = 3;
const SEED: u8 = 4;
const RUN_OWN: u8 = 5;

fn os(bytes: &[u8]) -> OsString {
    OsString::from_vec(bytes.to_vec())
}

fn address(name: &OsStr) -> io::Result<SocketAddr> {
    SocketAddr::from_abstract_name(name.as_bytes())
}

// ---- the wrapper's side ---------------------------------------------------

/// Hand a compiler command to the build's supervisor. None if there is no
/// supervisor to ask, in which case the caller handles the command alone.
pub fn ask(session: &OsStr, program: &OsStr, args: &[OsString], tool: bool) -> Result<Option<i32>> {
    let Ok(mut stream) = address(session).and_then(|addr| UnixStream::connect_addr(&addr)) else {
        return Ok(None);
    };
    let mut w = Writer(vec![REQ_COMPILER]);
    w.number(std::process::id().into());
    w.0.push(tool.into());
    w.0.push(command::descriptor_spare().into());
    w.bytes(std::env::current_dir()?.as_os_str().as_bytes());
    w.bytes(program.as_bytes());
    w.number(args.len() as u64);
    for arg in args {
        w.bytes(arg.as_bytes());
    }
    let vars: Vec<_> = command::own_environment().collect();
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
            let place = place(r.byte()? != 0);
            let held = r.optional()?.map(|dir| PathBuf::from(os(dir)));
            let mount_real = r.optional()?.map(|dir| PathBuf::from(os(dir)));
            let mount_mp = r.optional()?.map(|dir| PathBuf::from(os(dir)));
            let mounted = mount_real.as_deref().zip(mount_mp.as_deref());
            let waited = place.as_ref().map_or(0, |p| p.waited.as_millis() as u64);
            // The supervisor is already watching this process's children.
            let ran = match command::run_piped(&argv, None, held.as_deref(), mounted) {
                Ok((ran, _)) => ran,
                // That command line needs the directory/mount it could not be
                // started with. Cargo's own does not.
                Err(_) if held.is_some() || mounted.is_some() => return Ok(None),
                Err(e) => return Err(e),
            };
            drop(place);
            let mut w = Writer(Vec::new());
            w.number(ran.code as u32 as u64);
            w.0.push(ran.success.into());
            w.optional(ran.stdout.as_deref());
            w.optional(ran.stderr.as_deref());
            w.number(ran.millis);
            w.number(waited);
            // The result is stored before Cargo is told the command is
            // done, so whatever Cargo starts next can find it.
            if wire::send(&mut stream, &w.0).is_ok() {
                let _ = wire::receive(&mut stream);
            }
            Ok(Some(ran.code))
        }
        CONTINUE => {
            let mut argv = Vec::new();
            for _ in 0..r.count()? {
                argv.push(os(r.bytes()?));
            }
            let place = place(r.byte()? != 0);
            let held = r.optional()?.map(|dir| PathBuf::from(os(dir)));
            let mount_real = r.optional()?.map(|dir| PathBuf::from(os(dir)));
            let mount_mp = r.optional()?.map(|dir| PathBuf::from(os(dir)));
            let mounted = mount_real.as_deref().zip(mount_mp.as_deref());
            drop(stream);
            let mut cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..]).env_remove("JATO_SESSION");
            let setup = if let Some((real, mp)) = mounted {
                match crate::mount::enter_for_exec(real, mp) {
                    Ok(()) => true,
                    Err(e) => {
                        log(
                            &program.to_string_lossy(),
                            format_args!("target mount skipped: {e}"),
                        );
                        false
                    }
                }
            } else if let Some(dir) = &held {
                command::hold(&mut cmd, dir).is_ok()
            } else {
                true
            };
            if !setup {
                cmd = Command::new(program);
                cmd.args(args).env_remove("JATO_SESSION");
            }
            if let Some(place) = place {
                cmd.env(crate::jobs::HELD, "1");
                place.keep_through_exec();
            }
            command::passthrough(cmd).map(Some)
        }
        SEED => {
            let mut argv = Vec::new();
            for _ in 0..r.count()? {
                argv.push(os(r.bytes()?));
            }
            let place = place(r.byte()? != 0);
            let held = r.optional()?.map(|dir| PathBuf::from(os(dir)));
            let mount_real = r.optional()?.map(|dir| PathBuf::from(os(dir)));
            let mount_mp = r.optional()?.map(|dir| PathBuf::from(os(dir)));
            let mounted = mount_real.as_deref().zip(mount_mp.as_deref());
            let waited = place.as_ref().map_or(0, |p| p.waited.as_millis() as u64);
            let mut cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..])
                .env_remove("JATO_SESSION")
                .env(crate::jobs::HELD, "1");
            let setup = if let Some((real, mp)) = mounted {
                crate::mount::attach(&mut cmd, real, mp).is_ok()
            } else if let Some(dir) = &held {
                command::hold(&mut cmd, dir).is_ok()
            } else {
                true
            };
            if !setup {
                drop(place);
                drop(stream);
                let mut fallback = Command::new(program);
                fallback.args(args).env_remove("JATO_SESSION");
                return command::passthrough(fallback).map(Some);
            }
            let status = match cmd.status() {
                Ok(status) => status,
                Err(e) if mounted.is_some() || held.is_some() => {
                    log(
                        &program.to_string_lossy(),
                        format_args!("target mount skipped: {e}"),
                    );
                    let mut fallback = Command::new(program);
                    fallback
                        .args(args)
                        .env_remove("JATO_SESSION")
                        .env(crate::jobs::HELD, "1");
                    fallback.status().context("starting the compiler")?
                }
                Err(e) => return Err(e).context("starting the compiler"),
            };
            drop(place);
            let mut w = Writer(Vec::new());
            w.0.push(status.success().into());
            w.number(waited);
            let _ = wire::send(&mut stream, &w.0);
            Ok(Some(command::exit_code(status)))
        }
        RUN_OWN => {
            let mut argv = Vec::new();
            for _ in 0..r.count()? {
                argv.push(os(r.bytes()?));
            }
            let place = place(r.byte()? != 0);
            let held = r.optional()?.map(|dir| PathBuf::from(os(dir)));
            let mount_real = r.optional()?.map(|dir| PathBuf::from(os(dir)));
            let mount_mp = r.optional()?.map(|dir| PathBuf::from(os(dir)));
            let mounted = mount_real.as_deref().zip(mount_mp.as_deref());
            let async_ok = r.byte()? != 0;
            let store_dir = PathBuf::from(os(r.bytes()?));
            let mut ignore = Vec::new();
            for _ in 0..r.count()? {
                ignore.push(PathBuf::from(os(r.bytes()?)));
            }
            let mut unlisted = Vec::new();
            for _ in 0..r.count()? {
                unlisted.push(PathBuf::from(os(r.bytes()?)));
            }
            let state = r.optional()?.map(|p| PathBuf::from(os(p)));
            let config = observe::Config {
                fast: observe::fast_library(&store_dir),
                ignore,
                unlisted,
                state,
                programs_in: None,
                mount: mounted.map(|(real, mp)| (mp.to_owned(), real.to_owned())),
            };
            let waited = place.as_ref().map_or(0, |p| p.waited.as_millis() as u64);
            let (ran, mut report) = match command::run_piped_config(
                &argv,
                Some(config),
                held.as_deref(),
                mounted,
                true,
            ) {
                Ok(done) => done,
                Err(e) => {
                    log(
                        &program.to_string_lossy(),
                        format_args!("not observed: {e:#}"),
                    );
                    drop(place);
                    drop(stream);
                    let mut fallback = Command::new(program);
                    fallback.args(args).env_remove("JATO_SESSION");
                    return command::passthrough(fallback).map(Some);
                }
            };
            drop(place);
            if async_ok && ran.success && report.incomplete.is_none() && !report.network {
                for (path, access) in &mut report.paths {
                    if access.kind == crate::observe::Kind::Absent
                        && !access.written
                        && !access.removed
                        && !access.created
                        && !access.appeared
                        && std::fs::metadata(path).is_ok()
                    {
                        access.appeared = true;
                    }
                }
                report.absent_verified = true;
            }
            let mut w = Writer(Vec::new());
            w.number(ran.code as u32 as u64);
            w.0.push(ran.success.into());
            w.optional(ran.stdout.as_deref());
            w.optional(ran.stderr.as_deref());
            w.number(ran.millis);
            w.number(waited);
            report.encode(&mut w);
            if wire::send(&mut stream, &w.0).is_ok() {
                let _ = wire::receive(&mut stream);
            }
            Ok(Some(ran.code))
        }
        _ => Ok(None),
    }
}

/// A place among the machine's running compilers, for a command that is
/// one.
fn place(compiles: bool) -> Option<crate::jobs::Place> {
    let cache = crate::cas::cache_dir().ok()?;
    compiles.then(|| crate::jobs::take(&cache))
}

// ---- the supervisor's side ------------------------------------------------

enum Mode {
    Watched {
        scopes: Scopes,
        parts: Box<Mutex<Parts>>,
        in_place_edit: bool,
    },
    Unwatched {
        cargo_exe: Option<(u64, u64)>,
    },
}

fn exe_id(pid: u32) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(format!("/proc/{pid}/exe")).ok()?;
    Some((meta.dev(), meta.ino()))
}

struct Server {
    mode: Mode,
    roots: Roots,
    /// The process Cargo was started in.
    cargo: u32,
    stores: Mutex<Vec<Store>>,
    shared_stamps: Arc<SharedStamps>,
    stop: AtomicBool,
    /// Requests still being served, some past the point where their
    /// compiler was told it is done.
    busy: std::sync::atomic::AtomicUsize,
    /// Caps concurrent asynchronous unit recording while Cargo is running so
    /// background hashing and compression never starve active compilations,
    /// then expands to all cores once Cargo exits so tail recordings drain
    /// immediately. Tuple is `(active, limit)`.
    recorders: (Mutex<(usize, usize)>, Condvar),
    /// Nested compiler/tool manifests produced inside active build-script scopes,
    /// keyed by the build-script wrapper PID.
    script_children: Mutex<std::collections::HashMap<u32, crate::scripts_cache::ScriptChildren>>,
}

struct RecorderSlot<'a>(&'a Server);

impl Drop for RecorderSlot<'_> {
    fn drop(&mut self) {
        let mut state = self.0.recorders.0.lock().unwrap_or_else(|e| e.into_inner());
        state.0 = state.0.saturating_sub(1);
        self.0.recorders.1.notify_one();
    }
}

impl Server {
    fn acquire_recorder(&self) -> RecorderSlot<'_> {
        let mut state = self.recorders.0.lock().unwrap_or_else(|e| e.into_inner());
        while state.0 >= state.1 {
            state = self
                .recorders
                .1
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
        state.0 += 1;
        RecorderSlot(self)
    }

    /// Expand background recording concurrency to all cores once Cargo exits.
    fn boost_recorders(&self) {
        let all = std::thread::available_parallelism()
            .map_or(16, |n| n.get())
            .max(16);
        let mut state = self.recorders.0.lock().unwrap_or_else(|e| e.into_inner());
        state.1 = all;
        self.recorders.1.notify_all();
    }

    fn store(&self) -> Result<Store> {
        let spare = self.stores.lock().unwrap_or_else(|e| e.into_inner()).pop();
        match spare {
            Some(store) => Ok(store),
            None => {
                let mut store = Store::open()?;
                store.set_shared_stamps(self.shared_stamps.clone());
                Ok(store)
            }
        }
    }

    fn return_store(&self, store: Store) {
        self.stores
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(store);
    }

    fn unusable(&self, why: &str) {
        if let Mode::Watched { parts, .. } = &self.mode {
            let mut parts = parts.lock().unwrap_or_else(|e| e.into_inner());
            parts.unusable.get_or_insert_with(|| why.to_owned());
        }
    }

    fn used(&self, from: &Roots, manifest: &crate::manifest::Manifest, cargo: bool, under: u16) {
        if let Mode::Watched { parts, .. } = &self.mode {
            let mut parts = parts.lock().unwrap_or_else(|e| e.into_inner());
            parts.add(manifest, from, &self.roots, cargo, under);
        }
    }

    fn note_script_child(
        &self,
        script_pid: u32,
        manifest: crate::manifest::Manifest,
        roots: Roots,
    ) {
        let mut map = self
            .script_children
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(children) = map.get_mut(&script_pid) {
            children.parts.push((manifest, roots));
        }
    }

    /// Whether the process that started `pid` is Cargo: runs a program
    /// the command itself is or has been, in the process Jato started or
    /// in one a subcommand of Cargo's started for the build. What Cargo
    /// starts as a compiler it runs again when the compiler's output is
    /// missing. What a build script starts is that script's affair.
    fn started_by_cargo(&self, pid: u32) -> bool {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        // After the name, which may hold anything: the state, then the
        // parent.
        let parent = stat
            .rsplit_once(") ")
            .and_then(|(_, rest)| rest.split(' ').nth(1)?.parse::<u32>().ok());
        parent.is_some_and(|parent| match &self.mode {
            Mode::Watched { scopes, .. } => parent == self.cargo || scopes.own_program(parent),
            Mode::Unwatched { cargo_exe } => {
                parent == self.cargo || cargo_exe.is_some_and(|exe| exe_id(parent) == Some(exe))
            }
        })
    }

    /// Serve one wrapper request (compiler/tool or build script) from its
    /// request to its last message.
    fn serve(&self, mut stream: UnixStream) -> Result<()> {
        let message = wire::receive(&mut stream)?;
        let mut r = Reader(&message);
        match r.byte()? {
            REQ_COMPILER => self.serve_compiler(stream, r),
            crate::scripts_cache::REQ_SCRIPT => self.serve_script(stream, r),
            _ => wire::send(&mut stream, &[DECLINE]).map_err(Into::into),
        }
    }

    fn serve_script(&self, mut stream: UnixStream, mut r: Reader<'_>) -> Result<()> {
        let pid = r.number()? as u32;
        let cwd = PathBuf::from(os(r.bytes()?));
        let binary = PathBuf::from(os(r.bytes()?));
        let mut args = Vec::new();
        for _ in 0..r.count()? {
            args.push(os(r.bytes()?));
        }
        let mut env = Env::new();
        for _ in 0..r.count()? {
            let name = os(r.bytes()?);
            env.insert(name, os(r.bytes()?));
        }
        let req = crate::scripts_cache::ScriptRequest {
            cwd,
            binary,
            args,
            env,
        };
        let label = crate::scripts_cache::script_label(&req);
        let mut store = match self.store() {
            Ok(store) => store,
            Err(e) => {
                log(&label, format_args!("no cache: {e:#}"));
                return wire::send(&mut stream, &[DECLINE]).map_err(Into::into);
            }
        };
        let Ok(meta) = std::fs::metadata(&req.binary) else {
            self.return_store(store);
            return wire::send(&mut stream, &[DECLINE]).map_err(Into::into);
        };
        let Ok(binary_digest) = store.digest_known(&req.binary, observe::Stamp::of(&meta)) else {
            self.return_store(store);
            return wire::send(&mut stream, &[DECLINE]).map_err(Into::into);
        };
        let script_roots =
            crate::scripts_cache::script_roots(&req, self.roots.path(crate::roots::TARGET));
        let recipe = crate::scripts_cache::script_recipe(&req, &script_roots, &binary_digest);
        let under = match &self.mode {
            Mode::Watched { scopes, .. } => scopes.under_program(pid),
            Mode::Unwatched { .. } => 0,
        };
        if let Ok(Some((outcome, manifest))) =
            crate::scripts_cache::lookup(&req, &script_roots, &recipe, &mut store)
        {
            if let Mode::Watched { scopes, .. } = &self.mode {
                self.used(&script_roots, &manifest, false, under);
                scopes.finish_script_process(pid);
            }
            self.return_store(store);
            let mut w = Writer(vec![HIT]);
            w.number(outcome.code as u32 as u64);
            w.bytes(&outcome.stdout);
            w.bytes(&outcome.stderr);
            return wire::send(&mut stream, &w.0).map_err(Into::into);
        }
        store.count("script_miss", 1);
        let mut ignore = vec![store.dir.clone()];
        ignore.extend(
            crate::jobs::TOLD_IN
                .iter()
                .filter_map(|name| req.env.get(OsStr::new(name)))
                .filter_map(|said| crate::jobs::named_pipe(said)),
        );
        match &self.mode {
            Mode::Watched { scopes, .. } => {
                self.script_children
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(pid, crate::scripts_cache::ScriptChildren::default());
                let obs = observe::Config {
                    fast: observe::fast_library(&store.dir),
                    ignore,
                    unlisted: Vec::new(),
                    state: None,
                    programs_in: None,
                    mount: None,
                };
                scopes.begin_script(pid, obs, under);
                let done =
                    wire::send(&mut stream, &[RUN]).and_then(|()| wire::receive(&mut stream));
                let report = scopes.end(pid).unwrap_or_default();
                let children = self
                    .script_children
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&pid)
                    .unwrap_or_default();
                let Ok(done) = done else {
                    scopes.absorb(report, under);
                    self.unusable("a build script was interrupted");
                    self.return_store(store);
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
                if ran.success
                    && let Err(why) = crate::scripts_cache::record(
                        &req,
                        &script_roots,
                        &recipe,
                        &ran,
                        &report,
                        &children,
                        &mut store,
                    )
                {
                    store.count("script_uncacheable", 1);
                    log(&label, format_args!("not stored: {why}"));
                }
                scopes.absorb(report, under);
                let _ = wire::send(&mut stream, &[0]);
            }
            Mode::Unwatched { .. } => {
                let mut w = Writer(vec![RUN_OWN]);
                w.bytes(store.dir.as_os_str().as_bytes());
                w.number(ignore.len() as u64);
                for p in &ignore {
                    w.bytes(p.as_os_str().as_bytes());
                }
                let done = wire::send(&mut stream, &w.0).and_then(|()| wire::receive(&mut stream));
                let Ok(done) = done else {
                    self.return_store(store);
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
                let report = Report::decode(&mut r)?;
                if ran.success
                    && let Err(why) = crate::scripts_cache::record(
                        &req,
                        &script_roots,
                        &recipe,
                        &ran,
                        &report,
                        &crate::scripts_cache::ScriptChildren::default(),
                        &mut store,
                    )
                {
                    store.count("script_uncacheable", 1);
                    log(&label, format_args!("not stored: {why}"));
                }
                let _ = wire::send(&mut stream, &[0]);
            }
        }
        self.return_store(store);
        Ok(())
    }

    fn serve_compiler(&self, mut stream: UnixStream, mut r: Reader<'_>) -> Result<()> {
        let pid = r.number()? as u32;
        let tool = r.byte()? != 0;
        let spare = r.byte()? != 0;
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
        // What a compiler starts is part of that compiler's work, and is
        // already being recorded as such. A build-script scope still allows
        // nested C compilers/assemblers to cache individually.
        if let Mode::Watched { scopes, .. } = &self.mode
            && tool
            && scopes.inside_compiler(pid)
        {
            return wire::send(&mut stream, &[DECLINE]).map_err(Into::into);
        }
        let cargo = !tool && self.started_by_cargo(pid);
        let enclosing_script = match &self.mode {
            Mode::Watched { scopes, .. } if !cargo => scopes.enclosing_script(pid),
            _ => None,
        };
        // The build script in whose doing a compiler was started that is
        // none of Cargo's own.
        let under = match (&self.mode, enclosing_script) {
            (_, Some((_, u))) => u,
            (Mode::Watched { scopes, .. }, None) if !cargo => scopes.under_program(pid),
            _ => 0,
        };
        let request = Request {
            program,
            args,
            cwd,
            env,
            spare: spare && !tool,
        };
        let mut job = if tool {
            Job::tool(request, self.roots.path(crate::roots::TARGET))
        } else {
            Job::unit(request, self.roots.path(crate::roots::TARGET))
        };
        let mut store = match self.store() {
            Ok(store) => store,
            Err(e) => {
                log(job.name(), format_args!("no cache: {e:#}"));
                self.unusable("a compiler ran without the cache");
                return wire::send(&mut stream, &[DECLINE]).map_err(Into::into);
            }
        };
        job.enable_mount(&store.dir);
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
            if let Some((script_pid, _)) = enclosing_script {
                self.note_script_child(script_pid, manifest.clone(), job.roots.clone());
            }
            self.used(&job.roots, &manifest, cargo, under);
            self.return_store(store);
            return reply(&mut stream, &outcome).map_err(Into::into);
        }
        match &self.mode {
            Mode::Watched {
                scopes,
                in_place_edit,
                ..
            } => {
                // Inside a build that is recorded whole, every compilation is.
                command::choose(&job, &mut store, &recipe, true, *in_place_edit);
                let argv = job.argv();
                let mut w = Writer(vec![RUN]);
                w.number(argv.len() as u64);
                for arg in &argv {
                    w.bytes(arg.as_bytes());
                }
                // Whether to wait for a processor before running it.
                w.0.push((job.tool || job.facts.compiles()).into());
                // And the directory to start it with on a descriptor, if any.
                w.optional(job.held().map(|dir| dir.as_os_str().as_bytes()));
                let mounted = job.mounted();
                w.optional(mounted.map(|(real, _)| real.as_os_str().as_bytes()));
                w.optional(mounted.map(|(_, mp)| mp.as_os_str().as_bytes()));
                store.count(job.counter("miss"), 1);
                scopes.begin(pid, job.observer(&store));
                let done = wire::send(&mut stream, &w.0).and_then(|()| wire::receive(&mut stream));
                let mut report = scopes.end(pid).unwrap_or_default();
                let Ok(done) = done else {
                    // The wrapper went away mid-compilation: the build was stopped.
                    self.unusable("a compiler was interrupted");
                    self.return_store(store);
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
                command::waited(
                    &job,
                    &mut store,
                    std::time::Duration::from_millis(r.number()?),
                );
                // For a successful rustc compilation started by Cargo itself
                // (not by a build script probe, which may delete its temp
                // output directory right after rustc exits), release Cargo
                // immediately so downstream crates start with zero
                // critical-path wait while we hash, stage, and commit the unit
                // in the background (still tracked by `server.busy` before the
                // whole build is recorded).
                let async_record = cargo
                    && ran.success
                    && !job.tool
                    && job.facts.compiles()
                    && job.facts.emits_link
                    && report.incomplete.is_none()
                    && !report.network
                    && std::env::var_os("JATO_ASYNC_RECORD").is_none_or(|v| v != "0");
                if async_record {
                    // Verify every absent path right at compiler exit before
                    // waking Cargo, so files created by subsequently scheduled
                    // crates cannot cause spurious "appeared while it ran"
                    // rejections while this unit stages in the background.
                    for (path, access) in &mut report.paths {
                        if access.kind == crate::observe::Kind::Absent
                            && !access.written
                            && !access.removed
                            && !access.created
                            && !access.appeared
                            && std::fs::metadata(path).is_ok()
                        {
                            access.appeared = true;
                        }
                    }
                    report.absent_verified = true;
                    let _ = wire::send(&mut stream, &[0]);
                    drop(stream);
                    let _slot = self.acquire_recorder();
                    let recorded =
                        match command::finish(&job, &mut store, &recipe, &ran, &report, None) {
                            Ok(manifest) => {
                                if let Some((script_pid, _)) = enclosing_script {
                                    self.note_script_child(
                                        script_pid,
                                        manifest.clone(),
                                        job.roots.clone(),
                                    );
                                }
                                self.used(&job.roots, &manifest, cargo, under);
                                Some(report)
                            }
                            Err(why) => {
                                self.unusable(&format!("{}: {why}", job.name()));
                                None
                            }
                        };
                    drop(lock);
                    if let Some(report) = recorded {
                        let left = command::Left::Seen(&report.state);
                        command::keep_state(&job, &mut store, &recipe, left);
                    }
                } else {
                    let recorded =
                        match command::finish(&job, &mut store, &recipe, &ran, &report, None) {
                            Ok(manifest) => {
                                if let Some((script_pid, _)) = enclosing_script {
                                    self.note_script_child(
                                        script_pid,
                                        manifest.clone(),
                                        job.roots.clone(),
                                    );
                                }
                                self.used(&job.roots, &manifest, cargo, under);
                                Some(report)
                            }
                            // A build goes on after a command that failed where
                            // the failure was an answer: a build script asking
                            // whether something compiles. And a build script's
                            // compiler whose result cannot be kept did what it
                            // did all the same. Either way it was the build's
                            // doing, as it would have been without a record of
                            // its own.
                            Err(_) if job.tool || !ran.success => {
                                if let Some((script_pid, _)) = enclosing_script {
                                    scopes.absorb_into(script_pid, report);
                                } else {
                                    scopes.absorb(report, under);
                                }
                                None
                            }
                            // A Rust compilation that worked but cannot be
                            // reused leaves nothing to say what it depended on.
                            Err(why) => {
                                self.unusable(&format!("{}: {why}", job.name()));
                                None
                            }
                        };
                    drop(lock);
                    let _ = wire::send(&mut stream, &[0]);
                    // Cargo has its answer and goes on. What the compiler
                    // saved for a next time is kept meanwhile.
                    if let Some(report) = recorded {
                        let left = command::Left::Seen(&report.state);
                        command::keep_state(&job, &mut store, &recipe, left);
                    }
                }
            }
            Mode::Unwatched { .. } => {
                let run = command::choose(&job, &mut store, &recipe, false, false);
                let argv = job.argv();
                let compiles = job.tool || job.facts.compiles();
                let held = job.held().map(|dir| dir.as_os_str().as_bytes());
                let mounted = job.mounted();
                let mount_real = mounted.map(|(real, _)| real.as_os_str().as_bytes());
                let mount_mp = mounted.map(|(_, mp)| mp.as_os_str().as_bytes());
                match run {
                    Run::Continued => {
                        log(job.name(), "left to rustc, unwatched and unrecorded");
                        store.count("continued", 1);
                        drop(lock);
                        let mut w = Writer(vec![CONTINUE]);
                        w.number(argv.len() as u64);
                        for arg in &argv {
                            w.bytes(arg.as_bytes());
                        }
                        w.0.push(compiles.into());
                        w.optional(held);
                        w.optional(mount_real);
                        w.optional(mount_mp);
                        let _ = wire::send(&mut stream, &w.0);
                    }
                    Run::Seeding => {
                        log(
                            job.name(),
                            "left to rustc, unwatched and unrecorded; the state it saves is kept",
                        );
                        store.count("continued", 1);
                        let since = std::time::SystemTime::now();
                        let mut w = Writer(vec![SEED]);
                        w.number(argv.len() as u64);
                        for arg in &argv {
                            w.bytes(arg.as_bytes());
                        }
                        w.0.push(compiles.into());
                        w.optional(held);
                        w.optional(mount_real);
                        w.optional(mount_mp);
                        let done =
                            wire::send(&mut stream, &w.0).and_then(|()| wire::receive(&mut stream));
                        drop(stream);
                        if let Ok(done) = done {
                            let mut r = Reader(&done);
                            let success = r.byte().unwrap_or(0) != 0;
                            if let Ok(waited_ms) = r.number() {
                                command::waited(
                                    &job,
                                    &mut store,
                                    std::time::Duration::from_millis(waited_ms),
                                );
                            }
                            if success {
                                let _slot = self.acquire_recorder();
                                command::keep_state(
                                    &job,
                                    &mut store,
                                    &recipe,
                                    command::Left::Since(since),
                                );
                            }
                        }
                        drop(lock);
                    }
                    Run::Recorded => {
                        store.count(job.counter("miss"), 1);
                        let async_ok = cargo
                            && !job.tool
                            && job.facts.compiles()
                            && job.facts.emits_link
                            && std::env::var_os("JATO_ASYNC_RECORD").is_none_or(|v| v != "0");
                        let obs = job.observer(&store);
                        let mut w = Writer(vec![RUN_OWN]);
                        w.number(argv.len() as u64);
                        for arg in &argv {
                            w.bytes(arg.as_bytes());
                        }
                        w.0.push(compiles.into());
                        w.optional(held);
                        w.optional(mount_real);
                        w.optional(mount_mp);
                        w.0.push(async_ok.into());
                        w.bytes(store.dir.as_os_str().as_bytes());
                        w.number(obs.ignore.len() as u64);
                        for p in &obs.ignore {
                            w.bytes(p.as_os_str().as_bytes());
                        }
                        w.number(obs.unlisted.len() as u64);
                        for p in &obs.unlisted {
                            w.bytes(p.as_os_str().as_bytes());
                        }
                        w.optional(obs.state.as_deref().map(|p| p.as_os_str().as_bytes()));
                        let done =
                            wire::send(&mut stream, &w.0).and_then(|()| wire::receive(&mut stream));
                        let Ok(done) = done else {
                            drop(lock);
                            self.return_store(store);
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
                        let waited_ms = r.number()?;
                        let report = Report::decode(&mut r)?;
                        command::waited(
                            &job,
                            &mut store,
                            std::time::Duration::from_millis(waited_ms),
                        );
                        if async_ok
                            && ran.success
                            && report.incomplete.is_none()
                            && !report.network
                            && report.absent_verified
                        {
                            let _ = wire::send(&mut stream, &[0]);
                            drop(stream);
                            let _slot = self.acquire_recorder();
                            if command::finish(&job, &mut store, &recipe, &ran, &report, None)
                                .is_ok()
                            {
                                command::keep_state(
                                    &job,
                                    &mut store,
                                    &recipe,
                                    command::Left::Seen(&report.state),
                                );
                            }
                            drop(lock);
                        } else {
                            let ok =
                                command::finish(&job, &mut store, &recipe, &ran, &report, None)
                                    .is_ok();
                            drop(lock);
                            let _ = wire::send(&mut stream, &[0]);
                            drop(stream);
                            if ok {
                                let _slot = self.acquire_recorder();
                                command::keep_state(
                                    &job,
                                    &mut store,
                                    &recipe,
                                    command::Left::Seen(&report.state),
                                );
                            }
                        }
                    }
                }
            }
        }
        self.return_store(store);
        Ok(())
    }
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
    let work = start.as_deref().and_then(crate::roots::workspace_above);
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

/// Words Cargo starts a line with to say what it is doing.
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

/// What kind of line Cargo printed.
#[derive(PartialEq)]
enum Line {
    Progress,
    Finished,
    Other,
}

/// Text without what sets its colour and weight on a terminal, and
/// without what makes part of it a link there.
fn unstyled(text: &[u8]) -> Vec<u8> {
    let mut plain = Vec::with_capacity(text.len());
    let mut bytes = text.iter().copied().peekable();
    while let Some(b) = bytes.next() {
        if b == 0x1b && bytes.peek() == Some(&b'[') {
            for c in bytes.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else if b == 0x1b && bytes.peek() == Some(&b']') {
            // Up to a bell, or to an escape and a backslash.
            while let Some(c) = bytes.next() {
                if c == 0x07 || (c == 0x1b && bytes.next_if_eq(&b'\\').is_some()) {
                    break;
                }
            }
        } else {
            plain.push(b);
        }
    }
    plain
}

/// What a terminal would show of a line, which is the text after the last
/// return to the start of the line, and what kind of line it is.
fn shown(line: &[u8]) -> (&[u8], Line) {
    let shown = line.rsplit(|b| *b == b'\r').find(|part| part.len() > 1);
    let shown = shown.unwrap_or(line);
    let plain = unstyled(shown);
    let text = String::from_utf8_lossy(&plain);
    let word = text.trim_start().split(' ').next().unwrap_or_default();
    let kind = if !text.starts_with(' ') {
        Line::Other
    } else if PROGRESS.contains(&word) {
        Line::Progress
    } else if word == "Finished" {
        Line::Finished
    } else {
        Line::Other
    };
    (shown, kind)
}

/// What Cargo would print for a build in which nothing had to be done,
/// taken from what it printed when everything was. Lines that report
/// progress go; diagnostics and the summary stay, with the time it took
/// this time. With `colour` unset, as it is where no terminal shows them,
/// they are printed without colours, as Cargo prints there. The summary
/// is in Cargo's colours on a terminal even if the build was recorded
/// without one.
fn replayed(recorded: &[u8], elapsed: f64, quiet: bool, colour: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(recorded.len());
    for line in recorded.split_inclusive(|b| *b == b'\n') {
        let (shown, kind) = shown(line);
        // Without what erased a progress bar that is not drawn this time.
        let shown = shown.strip_prefix(b"\x1b[K").unwrap_or(shown);
        match kind {
            Line::Progress => (),
            Line::Finished => {
                if !quiet && let Some(at) = memchr::memmem::rfind(shown, b" in ") {
                    let plain = unstyled(&shown[..at]);
                    let said = plain.trim_ascii_start();
                    let said = said.strip_prefix(b"Finished").unwrap_or(said);
                    out.extend_from_slice(if colour {
                        b"\x1b[1m\x1b[92m    Finished\x1b[0m"
                    } else {
                        b"    Finished"
                    });
                    out.extend_from_slice(said);
                    out.extend_from_slice(format!(" in {elapsed:.2}s\n").as_bytes());
                }
            }
            Line::Other if colour => out.extend_from_slice(shown),
            Line::Other => out.extend_from_slice(&unstyled(shown)),
        }
    }
    out
}

/// Passes on the lines in which Cargo says what it is compiling, and
/// nothing else. For a build made ahead of the command that was asked
/// for: that command says the rest itself when it finds the build done.
struct ProgressOnly<W: Write> {
    to: W,
    line: Vec<u8>,
}

impl<W: Write> Write for ProgressOnly<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for part in bytes.split_inclusive(|b| *b == b'\n') {
            self.line.extend_from_slice(part);
            if self.line.last() != Some(&b'\n') {
                break;
            }
            let (text, kind) = shown(&self.line);
            if kind == Line::Progress {
                // Without what erases a progress bar drawn before it.
                let text = text.strip_prefix(b"\x1b[K").unwrap_or(text);
                self.to.write_all(text)?;
            }
            self.line.clear();
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.to.flush()
    }
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
    let mut env: Env = command::own_environment().collect();
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
        spare: false,
    };
    let (roots, skipped) = locate(&request);
    // Where a build is shown is no part of which build it is. Cargo has
    // the compilers colour their diagnostics whether or not it will show
    // them in colour, and shows those of a crate it finds up to date as
    // they were laid out when the crate was compiled, however wide the
    // terminal was then.
    let mut salt = Vec::new();
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

/// Run a Cargo command that only builds, or fill in what it would build.
pub fn build(cmd: Command, name: &str) -> Result<i32> {
    let begun = Instant::now();
    let Some(Prepared {
        job,
        mut store,
        recipe,
    }) = prepare(&cmd, name)?
    else {
        return command::passthrough(cmd);
    };
    let mut cache = LookupCache::default();
    match command::reuse_cached(&job, &mut store, &recipe, true, Some(&mut cache)) {
        Ok(Some((outcome, _))) => {
            // Cargo reports each artifact it did not have to rebuild.
            let fresh = String::from_utf8_lossy(&outcome.stdout)
                .replace("\"fresh\":false", "\"fresh\":true");
            io::stdout().write_all(fresh.as_bytes())?;
            let quiet = cmd.get_args().any(|a| a == "-q" || a == "--quiet");
            let elapsed = begun.elapsed().as_secs_f64();
            let colour = io::stderr().is_terminal();
            io::stderr().write_all(&replayed(&outcome.stderr, elapsed, quiet, colour))?;
            return Ok(outcome.code);
        }
        Ok(None) => (),
        Err(e) => log(name, format_args!("lookup failed: {e:#}")),
    }
    // An edit in a target that is already built: Cargo runs unwatched and
    // checks everything natively, while compiler wrappers talk to a
    // lightweight supervisor sharing pooled SQLite stores, an in-memory stamp
    // cache with per-inode singleflight deduplication, and bounded background
    // recording.
    if command::mostly_built_cached(&job, &store, &recipe, Some(&mut cache)) {
        if let Some(start) =
            command::continue_from_cached(&job, &mut store, &recipe, Some(&mut cache))
        {
            let again = copy_of(&cmd);
            let prepared = Prepared { job, store, recipe };
            return match supervise(cmd, prepared, name, begun, true, Some(start))? {
                Supervised::Ran(code) => Ok(code),
                Supervised::Unwatched(cmd) => command::passthrough(cmd),
                Supervised::Doubted => match prepare(&again, name)? {
                    Some(prepared) => match supervise(again, prepared, name, begun, true, None)? {
                        Supervised::Ran(code) => Ok(code),
                        Supervised::Unwatched(cmd) => command::passthrough(cmd),
                        Supervised::Doubted => bail!("a build from nothing cannot be in doubt"),
                    },
                    None => command::passthrough(again),
                },
            };
        }
        log(
            name,
            "the target is already built: leaving this build to Cargo",
        );
        store.count("build_left_to_cargo", 1);
        return supervise_unwatched(cmd, job, store);
    }
    let prepared = Prepared { job, store, recipe };
    match make(cmd, prepared, name, begun, true, Some(&mut cache))? {
        Supervised::Ran(code) => Ok(code),
        Supervised::Unwatched(cmd) => command::passthrough(cmd),
        Supervised::Doubted => bail!("a build from nothing cannot be in doubt"),
    }
}

/// Run Cargo completely unwatched (for a target that is already mostly built)
/// while serving compiler wrappers through a lightweight in-process supervisor
/// that pools SQLite connections, deduplicates large-file hashing across
/// concurrent crates, and runs unit recording asynchronously behind a bounded
/// worker pool.
fn supervise_unwatched(mut cmd: Command, job: Job, mut store: Store) -> Result<i32> {
    let socket = format!(
        "jato-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let listener =
        match address(OsStr::new(&socket)).and_then(|addr| UnixListener::bind_addr(&addr)) {
            Ok(listener) => listener,
            Err(_) => {
                drop(store);
                return command::passthrough(cmd);
            }
        };
    cmd.env("JATO_SESSION", &socket);
    if let Some(target) = job.roots.path(crate::roots::TARGET) {
        crate::scripts_cache::offer(&mut cmd, &store.dir, target);
    }
    let tokens = crate::jobs::offer(&mut cmd, &store.dir);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(_) => {
            crate::jobs::withdraw(&mut cmd, tokens);
            crate::scripts_cache::withdraw(&mut cmd);
            cmd.env_remove("JATO_SESSION");
            drop(store);
            return command::passthrough(cmd);
        }
    };
    let pid = child.id();
    let cargo_exe = exe_id(pid);
    let shared_stamps = Arc::new(SharedStamps::new());
    store.set_shared_stamps(shared_stamps.clone());
    let recorder_limit =
        (std::thread::available_parallelism().map_or(4, |n| n.get()) / 4).clamp(2, 8);
    let server = Arc::new(Server {
        mode: Mode::Unwatched { cargo_exe },
        roots: job.roots.clone(),
        cargo: pid,
        stores: Mutex::new(Vec::new()),
        shared_stamps,
        stop: AtomicBool::new(false),
        busy: std::sync::atomic::AtomicUsize::new(0),
        recorders: (Mutex::new((0, recorder_limit)), Condvar::new()),
        script_children: Mutex::new(std::collections::HashMap::new()),
    });
    let accepting = server.clone();
    let accept = std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if accepting.stop.load(Ordering::Acquire) {
                break;
            }
            if !wire::same_user(&stream) {
                continue;
            }
            let server = accepting.clone();
            server.busy.fetch_add(1, Ordering::AcqRel);
            std::thread::spawn(move || {
                let _ = server.serve(stream);
                server.busy.fetch_sub(1, Ordering::AcqRel);
            });
        }
    });
    let kill_pid = pid as i32;
    let _ = ctrlc::set_handler(move || {
        STOPPED.store(true, Ordering::Release);
        unsafe { libc::kill(kill_pid, libc::SIGTERM) };
    });
    let status = child.wait().context("waiting for Cargo")?;
    drop(tokens);
    server.stop.store(true, Ordering::Release);
    server.boost_recorders();
    if let Ok(addr) = address(OsStr::new(&socket)) {
        let _ = UnixStream::connect_addr(&addr);
    }
    let _ = accept.join();
    let patience = Instant::now();
    while server.busy.load(Ordering::Acquire) > 0 && patience.elapsed().as_secs() < 60 {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    server
        .stores
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    store.settle();
    Ok(command::exit_code(status))
}

/// Have Cargo make a build that is not recorded, under one observer. If
/// sources that differ only a little were built before, the build starts
/// from that one. Should Cargo then leave unmade what had to be made again,
/// everything put in place is taken away and the build is made from
/// nothing.
fn make(
    cmd: Command,
    prepared: Prepared,
    name: &str,
    begun: Instant,
    whole: bool,
    cache: Option<&mut LookupCache>,
) -> Result<Supervised> {
    let Prepared {
        job,
        mut store,
        recipe,
    } = prepared;
    let start = command::start_from_cached(&job, &mut store, &recipe, cache)
        .or_else(|| command::rest_on(&job, &mut store, &recipe))
        .or_else(|| command::seed_from(&job, &mut store, &recipe));
    let again = start.is_some().then(|| copy_of(&cmd));
    let prepared = Prepared { job, store, recipe };
    match supervise(cmd, prepared, name, begun, whole, start)? {
        Supervised::Doubted => {
            let Some(cmd) = again else {
                bail!("the build could not be started again");
            };
            match prepare(&cmd, name)? {
                Some(prepared) => supervise(cmd, prepared, name, begun, whole, None),
                None => Ok(Supervised::Unwatched(cmd)),
            }
        }
        other => Ok(other),
    }
}

/// The same command, to be started a second time.
fn copy_of(cmd: &Command) -> Command {
    let mut copy = Command::new(cmd.get_program());
    copy.args(cmd.get_args());
    for (name, value) in cmd.get_envs() {
        match value {
            Some(value) => copy.env(name, value),
            None => copy.env_remove(name),
        };
    }
    if let Some(dir) = cmd.get_current_dir() {
        copy.current_dir(dir);
    }
    copy
}

/// Get the build done that a command needs before it runs what was built:
/// `cmd` is that command cut down to its build. A recorded build is filled
/// in without a word. One that has to be made is made here, where it can be
/// recorded as a whole, showing only what is being compiled; the command
/// itself then finds it done and reports on it as it always does.
pub fn ready(cmd: Command, name: &str) {
    let begun = Instant::now();
    let Ok(Some(Prepared {
        job,
        mut store,
        recipe,
    })) = prepare(&cmd, name)
    else {
        return;
    };
    let mut cache = LookupCache::default();
    match command::reuse_cached(&job, &mut store, &recipe, true, Some(&mut cache)) {
        Ok(Some(_)) => return,
        Ok(None) => (),
        Err(e) => log(name, format_args!("lookup failed: {e:#}")),
    }
    if command::mostly_built_cached(&job, &store, &recipe, Some(&mut cache)) {
        if let Some(start) =
            command::continue_from_cached(&job, &mut store, &recipe, Some(&mut cache))
        {
            let again = copy_of(&cmd);
            let prepared = Prepared { job, store, recipe };
            let res = match supervise(cmd, prepared, name, begun, false, Some(start)) {
                Ok(Supervised::Doubted) => prepare(&again, name).and_then(|p| match p {
                    Some(prepared) => supervise(again, prepared, name, begun, false, None),
                    None => Ok(Supervised::Unwatched(again)),
                }),
                other => other,
            };
            if let Err(e) = res {
                log(
                    name,
                    format_args!("the build ahead of the command failed: {e:#}"),
                );
            }
            if STOPPED.load(Ordering::Acquire) {
                std::process::exit(130);
            }
        }
        return;
    }
    let prepared = Prepared { job, store, recipe };
    if let Err(e) = make(cmd, prepared, name, begun, false, Some(&mut cache)) {
        log(
            name,
            format_args!("the build ahead of the command failed: {e:#}"),
        );
    }
    // Whoever stopped the build did not mean for the command to start.
    if STOPPED.load(Ordering::Acquire) {
        std::process::exit(130);
    }
}

/// Set when this process was asked to stop while a build it supervises ran.
static STOPPED: AtomicBool = AtomicBool::new(false);

/// How a build that was to be watched went.
enum Supervised {
    /// To the end, with this exit status.
    Ran(i32),
    /// It could not be watched and has not been started.
    Unwatched(Command),
    /// Cargo called it done without making again what had to be. What it
    /// started from has been taken out of the target directory.
    Doubted,
}

/// Run Cargo under one observer and record the build as a whole. With
/// `whole` unset only Cargo's progress is shown, and what it printed is
/// kept for the record alone. `start` is an earlier build whose files have
/// been put in place for this one to start from.
fn supervise(
    mut cmd: Command,
    prepared: Prepared,
    name: &str,
    begun: Instant,
    whole: bool,
    start: Option<command::Start>,
) -> Result<Supervised> {
    let Prepared {
        job,
        mut store,
        recipe,
    } = prepared;
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
    let mut ignore = vec![store.dir.clone()];
    // Tokens that whoever started this build shares with it.
    ignore.extend(
        crate::jobs::TOLD_IN
            .iter()
            .filter_map(std::env::var_os)
            .filter_map(|said| crate::jobs::named_pipe(&said)),
    );
    let config = observe::Config {
        fast: observe::fast_library(&store.dir),
        ignore,
        unlisted: Vec::new(),
        state: None,
        // The build scripts Cargo runs are told apart, each with what it
        // and everything it starts examines and writes.
        programs_in: job.roots.path(crate::roots::TARGET).map(Path::to_owned),
        mount: None,
    };
    let mut fallback = copy_of(&cmd);
    if let Some(target) = job.roots.path(crate::roots::TARGET) {
        crate::scripts_cache::offer(&mut cmd, &store.dir, target);
    }
    // And the tokens for this build's own compilers, kept in the cache
    // directory until Cargo has gone.
    let tokens = crate::jobs::offer(&mut cmd, &store.dir);
    let (mut child, session) = match observe::spawn(&mut cmd, config) {
        Ok(started) => started,
        Err(e) => {
            log(name, format_args!("not observed: {e:#}"));
            drop(tokens);
            fallback
                .env_remove("JATO_SESSION")
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
            return Ok(Supervised::Unwatched(fallback));
        }
    };
    // The command holds the other end of the terminal pair. Keeping a copy
    // here would hide its closing.
    let child_err = child.stderr.take();
    drop(cmd);
    let shared_stamps = Arc::new(SharedStamps::new());
    store.set_shared_stamps(shared_stamps.clone());
    let recorder_limit =
        (std::thread::available_parallelism().map_or(4, |n| n.get()) / 4).clamp(2, 8);
    let in_place_edit = start.as_ref().is_some_and(|s| s.in_place);
    let server = Arc::new(Server {
        mode: Mode::Watched {
            scopes: session.scopes(),
            parts: Box::new(Mutex::new(Parts::starting_from(start))),
            in_place_edit,
        },
        roots: job.roots.clone(),
        cargo: child.id(),
        stores: Mutex::new(Vec::new()),
        shared_stamps,
        stop: AtomicBool::new(false),
        busy: std::sync::atomic::AtomicUsize::new(0),
        recorders: (Mutex::new((0, recorder_limit)), Condvar::new()),
        script_children: Mutex::new(std::collections::HashMap::new()),
    });
    let accepting = server.clone();
    let accept = std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if accepting.stop.load(Ordering::Acquire) {
                break;
            }
            if !wire::same_user(&stream) {
                continue;
            }
            let server = accepting.clone();
            server.busy.fetch_add(1, Ordering::AcqRel);
            std::thread::spawn(move || {
                if let Err(e) = server.serve(stream) {
                    server.unusable(&format!("a compiler's request failed: {e:#}"));
                }
                server.busy.fetch_sub(1, Ordering::AcqRel);
            });
        }
    });
    // Stopping this process alone would leave the compilers waiting for
    // answers to their system calls. Pass the signal on and wait.
    let pid = child.id() as i32;
    let _ = ctrlc::set_handler(move || {
        STOPPED.store(true, Ordering::Release);
        unsafe { libc::kill(pid, libc::SIGTERM) };
    });
    let out = child.stdout.take().unwrap();
    let copy_out = std::thread::spawn(move || {
        if whole {
            command::tee(out, io::stdout())
        } else {
            command::tee(out, io::sink())
        }
    });
    let from: Box<dyn io::Read> = match (master, child_err) {
        (Some(master), _) => Box::new(std::fs::File::from(master)),
        (None, Some(err)) => Box::new(err),
        (None, None) => bail!("the command's output could not be read"),
    };
    let (stderr, streamed) = if whole {
        command::tee(from, io::stderr())
    } else {
        let progress = ProgressOnly {
            to: io::stderr(),
            line: Vec::new(),
        };
        command::tee(from, progress)
    };
    let (stdout, streamed_out) = copy_out
        .join()
        .unwrap_or((None, Err(io::Error::other("output thread failed"))));
    if streamed.and(streamed_out).is_err() {
        let _ = child.kill();
    }
    let status = child.wait().context("waiting for Cargo")?;
    drop(tokens);
    server.stop.store(true, Ordering::Release);
    server.boost_recorders();
    let _ = UnixStream::connect_addr(&address(OsStr::new(&socket))?);
    let _ = accept.join();
    // Cargo has gone, and so have the compilers. Whatever is still being
    // kept for them is given a moment to finish.
    let patience = Instant::now();
    while server.busy.load(Ordering::Acquire) > 0 && patience.elapsed().as_secs() < 60 {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let report = session.finish();
    let ran = Ran {
        code: command::exit_code(status),
        success: status.success(),
        stdout,
        stderr,
        millis: begun.elapsed().as_millis() as u64,
    };
    let Mode::Watched { parts, .. } = &server.mode else {
        unreachable!();
    };
    let parts = parts.lock().unwrap_or_else(|e| e.into_inner());
    log(
        name,
        format_args!(
            "ran, with {} file accesses of its own, {} of {} calls told of by the programs themselves",
            report.paths.len(),
            report.noted,
            report.events
        ),
    );
    // Cargo runs a compiler again when what it wrote is missing. Nothing
    // else stands between a changed source and what was built from the
    // unchanged one, so it is checked that Cargo did.
    let unmade = parts
        .start
        .as_ref()
        .filter(|_| ran.success)
        .and_then(|start| {
            // The tests name a file here to go through what follows.
            let pretended = || std::env::var_os("JATO_TEST_DOUBT").map(PathBuf::from);
            start
                .unmade(&job.roots, &report)
                .or_else(|| start.unrun(&job.roots, &report))
                .or_else(pretended)
        });
    if let Some(unmade) = unmade {
        log(
            name,
            format_args!(
                "Cargo did not make or run {} again: taking away what this build started from, and building from nothing",
                unmade.display()
            ),
        );
        if let Some(start) = &parts.start {
            start.undo(&job.roots, &report, &parts);
        }
        drop(parts);
        server
            .stores
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        store.count("build_doubted", 1);
        store.settle();
        return Ok(Supervised::Doubted);
    }
    // Flush all pooled compiler stores before recording the whole build so
    // their learned stamps and counters are committed and visible.
    server
        .stores
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    let _ = command::finish(&job, &mut store, &recipe, &ran, &report, Some(&parts));
    drop(parts);
    store.settle();
    Ok(Supervised::Ran(ran.code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_restored_build_reads_like_one_with_nothing_to_do() {
        let recorded = [
            "\x1b[1m\x1b[92m   Compiling\x1b[0m leaf v0.1.0 (/w/leaf)\n",
            "    Building [>   ] 0/2: leaf\r\x1b[Kwarning: unused variable: `x`\n",
            " --> leaf/src/lib.rs:1:5\n",
            "\n",
            "warning: `leaf` (lib) generated 1 warning\n",
            "    Building [=====>   ] 1/2: app\r\x1b[K   Compiling app v0.1.0 (/w)\n",
            "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 12.50s\n",
        ]
        .concat();
        let text = String::from_utf8(replayed(recorded.as_bytes(), 0.031, false, false)).unwrap();
        let expected = [
            "warning: unused variable: `x`\n",
            " --> leaf/src/lib.rs:1:5\n",
            "\n",
            "warning: `leaf` (lib) generated 1 warning\n",
            "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.03s\n",
        ];
        assert_eq!(text, expected.concat());
        let quiet = String::from_utf8(replayed(recorded.as_bytes(), 0.031, true, false)).unwrap();
        assert_eq!(quiet, expected[..expected.len() - 1].concat());
        // Text that merely starts with such a word is a diagnostic.
        assert_eq!(
            replayed(b"Running out of ideas\n", 1.0, false, false),
            b"Running out of ideas\n"
        );
        // The summary is styled for where it is shown now, whichever way
        // it was recorded.
        let plain = b"    Finished `dev` profile [unoptimized] target(s) in 3.00s\n";
        let styled =
            b"\x1b[1m\x1b[92m    Finished\x1b[0m `dev` profile [unoptimized] target(s) in 3.00s\n";
        for recorded in [&plain[..], &styled[..]] {
            assert_eq!(
                replayed(recorded, 0.5, false, false),
                b"    Finished `dev` profile [unoptimized] target(s) in 0.50s\n"
            );
            assert_eq!(
                replayed(recorded, 0.5, false, true),
                b"\x1b[1m\x1b[92m    Finished\x1b[0m `dev` profile [unoptimized] target(s) in 0.50s\n"
            );
        }
        // A diagnostic recorded on a terminal, shown where there is none.
        let warning = b"\x1b[1m\x1b[33mwarning\x1b[0m: unused, see \x1b]8;;https://example.invalid\x1b\\E0001\x1b]8;;\x1b\\\n";
        assert_eq!(replayed(warning, 1.0, false, true), warning);
        assert_eq!(
            replayed(warning, 1.0, false, false),
            b"warning: unused, see E0001\n"
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
            spare: false,
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
