#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod events;
#[cfg(not(target_os = "linux"))]
#[path = "portable.rs"]
mod events;
mod tree;
mod workspace;

use crate::{CargoOptions, cargo_command, compiler};
use anyhow::{Context, Result, bail, ensure};
use std::collections::{BTreeSet, VecDeque};
use std::env;
use std::ffi::OsString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixDatagram;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Child;
use std::sync::Arc;
#[cfg(not(target_os = "linux"))]
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
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
    cargo: CargoOptions,
    focus_only: bool,
    delay: Option<Duration>,
    extra: Vec<PathBuf>,
    job: Vec<OsString>,
}

impl Options {
    fn parse(args: &[OsString], cargo: CargoOptions) -> Result<Self> {
        let mut opts = Self {
            cargo,
            focus_only: false,
            delay: None,
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
                    opts.delay = Some(Duration::from_millis(ms));
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
    new_dirs: BTreeSet<PathBuf>,
    completed: Option<PathBuf>,
}

impl Changes {
    fn all() -> Self {
        Self {
            full: true,
            paths: BTreeSet::new(),
            new_dirs: BTreeSet::new(),
            completed: None,
        }
    }
    fn add(&mut self, path: PathBuf, created_dir: bool) {
        if self.full {
            return;
        }
        if created_dir && !self.paths.contains(&path) {
            self.new_dirs.insert(path.clone());
        }
        if self.completed.as_ref() == Some(&path) {
            self.completed = None;
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
                let created = other.new_dirs.contains(&p);
                self.add(p, created);
            }
            if other.completed.is_some() {
                self.completed = other.completed;
            }
        }
    }
    fn empty(&self) -> bool {
        !self.full && self.paths.is_empty()
    }

    fn debounce(&self, idle: bool) -> Duration {
        if self.full || self.paths.len() > 4 || self.paths.iter().any(|p| configuration(p)) {
            Duration::from_millis(100)
        } else if idle && self.paths.len() == 1 && self.completed.as_ref() == self.paths.first() {
            Duration::ZERO
        } else {
            Duration::from_millis(16)
        }
    }
}

struct Inbox {
    #[cfg(not(target_os = "linux"))]
    changes: Mutex<Changes>,
    wake: UnixDatagram,
    stop: AtomicBool,
}

#[cfg(not(target_os = "linux"))]
impl Inbox {
    fn record(&self, paths: Vec<PathBuf>, created_dir: bool, completed: Option<PathBuf>) {
        let mut changes = self.changes.lock().unwrap();
        let wake = changes.empty() && changes.completed.is_none();
        for p in paths {
            changes.add(p, created_dir);
        }
        if let Some(path) = completed {
            changes.completed = Some(path);
        }
        if wake {
            let _ = self.wake.send(&[1]);
        }
    }

    fn rescan(&self) {
        *self.changes.lock().unwrap() = Changes::all();
        let _ = self.wake.send(&[1]);
    }
}

struct Job {
    child: Child,
    start: Instant,
    completion: Option<OwnedFd>,
}

impl Job {
    fn spawn(args: &[OsString], cargo: CargoOptions) -> Result<Self> {
        eprintln!(
            "jato watch > cargo {}",
            args.iter()
                .map(|a| a.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ")
        );
        let child = cargo_command(args, cargo)?
            .process_group(0)
            .spawn()
            .context("starting Cargo job")?;
        #[cfg(target_os = "linux")]
        let completion = {
            // A pidfd becomes readable at exit, even if Cargo finished before
            // poll starts. It stays associated with this particular child.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) } as i32;
            (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
        };
        #[cfg(not(target_os = "linux"))]
        let completion = None;
        Ok(Self {
            completion,
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
        if let Some(fd) = &self.completion {
            let mut pfd = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let _ = poll(std::slice::from_mut(&mut pfd), Some(deadline));
        } else {
            while Instant::now() < deadline {
                if self.child.try_wait().ok().flatten().is_some() {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
        unsafe {
            libc::kill(group, libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

fn poll(fds: &mut [libc::pollfd], deadline: Option<Instant>) -> std::io::Result<()> {
    loop {
        let millis = deadline.map_or(-1, |d| {
            d.saturating_duration_since(Instant::now())
                .as_nanos()
                .div_ceil(1_000_000)
                .min(i32::MAX as u128) as i32
        });
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, millis) };
        if ready >= 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

fn wait(
    receiver: &UnixDatagram,
    filesystem: std::os::fd::RawFd,
    job: Option<&Job>,
    mut deadline: Option<Instant>,
) -> std::io::Result<()> {
    let child = job.and_then(|j| j.completion.as_ref());
    if job.is_some() && child.is_none() {
        let fallback = Instant::now() + Duration::from_millis(40);
        deadline = Some(deadline.map_or(fallback, |d| d.min(fallback)));
    }
    let mut fds = [
        libc::pollfd {
            fd: receiver.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: child.map_or(-1, AsRawFd::as_raw_fd),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: filesystem,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    poll(&mut fds, deadline)?;
    if fds[0].revents != 0 {
        let _ = receiver.recv(&mut [0]);
    }
    Ok(())
}

pub fn run(args: &[OsString], cargo: CargoOptions) -> Result<i32> {
    if args == ["--help"] || args == ["-h"] {
        print!("{HELP}");
        return Ok(0);
    }
    let options = Options::parse(args, cargo)?;
    let (wake, receiver) = UnixDatagram::pair()?;
    wake.set_nonblocking(true)?;
    receiver.set_nonblocking(true)?;
    let inbox = Arc::new(Inbox {
        #[cfg(not(target_os = "linux"))]
        changes: Mutex::default(),
        wake,
        stop: AtomicBool::new(false),
    });
    let signal = inbox.clone();
    ctrlc::set_handler(move || {
        signal.stop.store(true, Ordering::Release);
        let _ = signal.wake.send(&[1]);
    })?;
    let mut workspace = Workspace::load(&options)?;
    let mut tree = Tree::new(&workspace, inbox.clone())?;
    eprintln!("jato watch: watching {}", env::current_dir()?.display());
    let mut handoff = Changes::default();
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
        let mut incoming = std::mem::take(&mut handoff);
        incoming.merge(tree.changes(&inbox)?);
        if !incoming.empty()
            || options.delay.is_none()
                && incoming
                    .completed
                    .as_ref()
                    .is_some_and(|p| batch.paths.contains(p))
        {
            batch.merge(incoming);
            let delay = options
                .delay
                .unwrap_or_else(|| batch.debounce(job.is_none() && queue.is_empty()));
            deadline = Some(Instant::now() + delay);
        }
        if deadline.is_some_and(|d| d <= Instant::now()) {
            tree.drop_transient_output_dirs(&mut batch);
            if batch.empty() {
                batch = Changes::default();
                deadline = None;
                continue;
            }
            if env::var_os("JATO_LOG").as_deref() == Some(std::ffi::OsStr::new("debug")) {
                if batch.full {
                    eprintln!("jato watch: filesystem requested a rescan");
                }
                for path in &batch.paths {
                    eprintln!("jato watch: changed {}", path.display());
                }
            }
            if let Some(mut old) = job.take() {
                old.stop();
                batch.merge(wave.clone());
            }
            if !queue.is_empty() {
                batch.merge(wave.clone());
            }
            let rescan = batch.full || batch.paths.iter().any(|p| configuration(p));
            if rescan || tree.topology_changed(&batch.paths) {
                match Workspace::load(&options).and_then(|ws| {
                    if rescan || !tree.extend(&ws, &batch.paths).unwrap_or(false) {
                        let replacement = Tree::new(&ws, inbox.clone())?;
                        // An edit can arrive on an old watch before its
                        // replacement has registered this particular path.
                        #[cfg(target_os = "linux")]
                        {
                            handoff = tree.changes(&inbox)?;
                        }
                        tree = replacement;
                    }
                    Ok(ws)
                }) {
                    Ok(ws) => workspace = ws,
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
            job = Some(Job::spawn(&args, cargo)?);
        }
        if handoff.empty() && handoff.completed.is_none() {
            wait(&receiver, tree.fd(), job.as_ref(), deadline)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_completed_single_save_can_start_an_idle_job() {
        let source = PathBuf::from("src/lib.rs");
        let mut batch = Changes {
            completed: Some(source.clone()),
            ..Changes::default()
        };
        assert!(batch.empty()); // A close without a write never schedules a build.
        batch.add(source.clone(), false);
        assert_eq!(batch.debounce(true), Duration::from_millis(16));
        batch.completed = Some(source);
        assert_eq!(batch.debounce(true), Duration::ZERO);
        assert_eq!(batch.debounce(false), Duration::from_millis(16));
        batch.add(PathBuf::from("Cargo.toml"), false);
        assert_eq!(batch.debounce(true), Duration::from_millis(100));
    }
}
