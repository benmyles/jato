mod tree;
mod workspace;

use crate::{cargo_command, compiler};
use anyhow::{Context, Result, bail, ensure};
use std::collections::{BTreeSet, VecDeque};
use std::env;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use tree::Tree;
use workspace::{Workspace, configuration};

pub const HELP: &str = "\
Usage: jato watch [--focus-only] [--debounce MS] [--watch PATH]... [-- CARGO_ARGS...]

Starts with cargo check --workspace. On a package edit, checks that package
first and then the workspace. --focus-only omits the follow-up workspace check.
A custom Cargo command runs verbatim on every change. Ctrl-C stops the job.

  jato watch
  jato watch --focus-only
  jato watch --watch ../schemas -- test --workspace
";

struct Options {
    focus_only: bool,
    delay: Duration,
    extra: Vec<PathBuf>,
    job: Vec<OsString>,
}

impl Options {
    fn parse(args: &[OsString]) -> Result<Self> {
        let mut opts = Self {
            focus_only: false,
            delay: Duration::from_millis(100),
            extra: Vec::new(),
            job: Vec::new(),
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--focus-only") => opts.focus_only = true,
                Some("--watch") => opts
                    .extra
                    .push(args.next().context("--watch needs a path")?.into()),
                Some("--debounce") => {
                    let ms: u64 = args
                        .next()
                        .and_then(|s| s.to_str())
                        .context("--debounce needs milliseconds")?
                        .parse()?;
                    ensure!(ms <= 60_000, "debounce must be at most 60000 ms");
                    opts.delay = Duration::from_millis(ms);
                }
                Some("--") => {
                    opts.job = args.cloned().collect();
                    ensure!(!opts.job.is_empty(), "expected Cargo arguments after --");
                    break;
                }
                _ => bail!("unknown watch argument {arg:?}. Run jato watch --help"),
            }
        }
        ensure!(
            opts.job.is_empty() || !opts.focus_only,
            "--focus-only applies to automatic package checks"
        );
        Ok(opts)
    }
}

#[derive(Clone, Default)]
struct Changes {
    full: bool,
    paths: BTreeSet<PathBuf>,
}

impl Changes {
    fn all() -> Self {
        Self {
            full: true,
            paths: BTreeSet::new(),
        }
    }
    fn add(&mut self, path: PathBuf) {
        if self.full {
            return;
        }
        self.paths.insert(path);
        if self.paths.len() >= 128 {
            *self = Self::all();
        }
    }
    fn merge(&mut self, other: Self) {
        if other.full {
            *self = Self::all();
        } else {
            for p in other.paths {
                self.add(p);
            }
        }
    }
    fn empty(&self) -> bool {
        !self.full && self.paths.is_empty()
    }
}

struct Inbox {
    changes: Mutex<Changes>,
    wake: mpsc::SyncSender<()>,
    stop: AtomicBool,
}

impl Inbox {
    fn record(&self, paths: Vec<PathBuf>) {
        let mut changes = self.changes.lock().unwrap();
        for p in paths {
            changes.add(p);
        }
        let _ = self.wake.try_send(());
    }

    fn rescan(&self) {
        *self.changes.lock().unwrap() = Changes::all();
        let _ = self.wake.try_send(());
    }
}

struct Job {
    child: Child,
    start: Instant,
}

impl Job {
    fn spawn(args: &[OsString]) -> Result<Self> {
        eprintln!(
            "jato watch > cargo {}",
            args.iter()
                .map(|a| a.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ")
        );
        let child = cargo_command(args)?
            .process_group(0)
            .spawn()
            .context("starting Cargo job")?;
        Ok(Self {
            child,
            start: Instant::now(),
        })
    }

    fn stop(&mut self) {
        let group = -(self.child.id() as i32);
        // We created this process group. Signal the whole group, including a
        // running test or rustc grandchild that inherited Cargo's pipes.
        unsafe {
            libc::kill(group, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_millis(250);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        unsafe {
            libc::kill(group, libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

pub fn run(args: &[OsString]) -> Result<i32> {
    if args == ["--help"] || args == ["-h"] {
        print!("{HELP}");
        return Ok(0);
    }
    let options = Options::parse(args)?;
    let (wake, receiver) = mpsc::sync_channel(1);
    let inbox = Arc::new(Inbox {
        changes: Mutex::default(),
        wake,
        stop: AtomicBool::new(false),
    });
    let signal = inbox.clone();
    ctrlc::set_handler(move || {
        signal.stop.store(true, Ordering::Release);
        let _ = signal.wake.try_send(());
    })?;
    let mut workspace = Workspace::load(&options.extra)?;
    let mut tree = Tree::new(&workspace, inbox.clone())?;
    eprintln!("jato watch: watching {}", env::current_dir()?.display());
    let mut batch = Changes::default();
    let mut deadline = None;
    let mut wave = Changes::all();
    let mut queue: VecDeque<_> = workspace.jobs(&wave, &options).into();
    let mut job: Option<Job> = None;
    loop {
        if inbox.stop.load(Ordering::Acquire) {
            if let Some(job) = job.as_mut() {
                job.stop();
            }
            return Ok(130);
        }
        let incoming = tree.filter(std::mem::take(&mut *inbox.changes.lock().unwrap()));
        if !incoming.empty() {
            batch.merge(incoming);
            deadline = Some(Instant::now() + options.delay);
        }
        if deadline.is_some_and(|d| d <= Instant::now()) {
            if let Some(mut old) = job.take() {
                old.stop();
                batch.merge(wave.clone());
            }
            if !queue.is_empty() {
                batch.merge(wave.clone());
            }
            if batch.full
                || batch.paths.iter().any(|p| configuration(p))
                || tree.topology_changed(&batch.paths)
            {
                match Workspace::load(&options.extra)
                    .and_then(|ws| Ok((Tree::new(&ws, inbox.clone())?, ws)))
                {
                    Ok((new_tree, ws)) => {
                        tree = new_tree;
                        workspace = ws;
                    }
                    Err(e) => {
                        eprintln!("jato watch: {e:#}");
                        batch = Changes::all();
                    }
                }
            }
            wave = std::mem::take(&mut batch);
            queue = workspace.jobs(&wave, &options).into();
            deadline = None;
        }
        if let Some(running) = &mut job
            && let Some(status) = running.child.try_wait()?
        {
            eprintln!(
                "jato watch: finished in {:.2}s (exit {})",
                running.start.elapsed().as_secs_f64(),
                compiler::exit_code(status)
            );
            job = None;
        }
        if job.is_none()
            && deadline.is_none()
            && let Some(args) = queue.pop_front()
        {
            job = Some(Job::spawn(&args)?);
        }
        let timeout = match (deadline, job.is_some()) {
            (Some(d), _) => Some(d.saturating_duration_since(Instant::now())),
            (None, true) => Some(Duration::from_millis(40)),
            (None, false) => None,
        };
        if let Some(timeout) = timeout {
            let _ = receiver.recv_timeout(timeout);
        } else {
            receiver.recv()?;
        }
    }
}
