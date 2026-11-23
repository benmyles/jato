//! Share the machine's processors among builds.
//!
//! Cargo runs as many compilers at once as there are processors. Five
//! builds started together run five times that, each compiler gets a
//! fifth of a processor and all of its memory, and every build takes far
//! longer than five in a row would have. Compilers started through Jato
//! take a place first. There are as many places as processors, shared by
//! every build that uses the same cache, so the builds queue for the
//! machine instead of dividing it.
//!
//! A place is a lock on a file, which the kernel gives up when its holder
//! ends in any way. A free place is taken without ceremony. When there is
//! none, those waiting line up behind a second lock, the turn, and the one
//! holding it looks for a place every few milliseconds.
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Set for a compiler that holds a place, so that a compiler it starts in
/// turn does not wait for a second one.
pub const HELD: &str = "JATO_JOB";

/// A place among the running compilers.
pub struct Place {
    file: Option<File>,
    /// How long it took to get.
    pub waited: Duration,
}

/// How many compilers may run at once. Zero means any number.
pub fn limit() -> usize {
    match std::env::var("JATO_JOBS") {
        Ok(value) => value.trim().parse().unwrap_or(0),
        Err(_) => std::thread::available_parallelism().map_or(0, |n| n.get()),
    }
}

fn lock(file: &File) -> io::Result<()> {
    loop {
        match file.lock() {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
            done => return done,
        }
    }
}

/// Wait for a place. Anything that goes wrong with the locks themselves
/// lets the compiler run: a build must never stop for want of bookkeeping.
pub fn take(cache: &Path) -> Place {
    if std::env::var_os(HELD).is_some() {
        return Place::free();
    }
    take_one_of(cache, limit())
}

fn take_one_of(cache: &Path, limit: usize) -> Place {
    if limit == 0 {
        return Place::free();
    }
    let dir = cache.join("locks").join("jobs");
    let open = |name: &str| {
        fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(name))
    };
    let begun = Instant::now();
    if fs::create_dir_all(&dir).is_err() {
        return Place::free();
    }
    let mut places: Vec<Option<File>> = (0..limit).map(|_| None).collect();
    // Compilers started together begin at different places.
    let first = std::process::id() as usize % limit;
    let look = |places: &mut Vec<Option<File>>| -> Result<Option<File>, ()> {
        for at in 0..limit {
            let i = (first + at) % limit;
            if places[i].is_none() {
                places[i] = open(&i.to_string()).ok();
            }
            let Some(file) = &places[i] else {
                return Err(());
            };
            match file.try_lock() {
                Ok(()) => return Ok(places[i].take()),
                Err(TryLockError::WouldBlock) => (),
                Err(TryLockError::Error(_)) => return Err(()),
            }
        }
        Ok(None)
    };
    // A free place is taken at once, with no lock that many compilers
    // starting together would have to pass through one by one.
    match look(&mut places) {
        Ok(Some(file)) => {
            return Place {
                file: Some(file),
                waited: begun.elapsed(),
            };
        }
        Ok(None) => (),
        Err(()) => return Place::free(),
    }
    // With none free, wait in line. Whoever is first looks every few
    // milliseconds, which is sooner than a build can start the compiler
    // that would take a place just given up.
    let Ok(turn) = open("turn") else {
        return Place::free();
    };
    if lock(&turn).is_err() {
        return Place::free();
    }
    loop {
        match look(&mut places) {
            Ok(Some(file)) => {
                return Place {
                    file: Some(file),
                    waited: begun.elapsed(),
                };
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(()) => return Place::free(),
        }
    }
}

impl Place {
    fn free() -> Self {
        Self {
            file: None,
            waited: Duration::ZERO,
        }
    }

    /// Keep the place for the program this process is about to become.
    pub fn keep_through_exec(mut self) {
        use std::os::fd::AsRawFd;
        if let Some(file) = self.file.take() {
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, 0) };
            std::mem::forget(file);
        }
    }
}

/// The variables in which a build is told of the tokens its compilers
/// share, as make tells its own: Cargo's first.
pub const TOLD_IN: [&str; 3] = ["CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS"];

/// The pipe a build's tokens are in, if it is one with a name: a file
/// that every compiler opens, reads and writes, and no input of any.
pub fn named_pipe(value: &OsStr) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let value = value.as_bytes();
    const SAYS: &[u8] = b"--jobserver-auth=fifo:";
    let at = memchr::memmem::rfind(value, SAYS)? + SAYS.len();
    let rest = &value[at..];
    let end = rest
        .iter()
        .position(u8::is_ascii_whitespace)
        .unwrap_or(rest.len());
    (end > 0).then(|| PathBuf::from(OsStr::from_bytes(&rest[..end])))
}

/// The tokens of one build: a pipe with a name, removed with this.
pub struct Tokens {
    path: PathBuf,
    /// Held open both ways, so that the tokens stay in the pipe while no
    /// compiler has it open and nobody waits to open it.
    _pipe: File,
}

impl Drop for Tokens {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Give the Cargo that `cmd` starts its tokens in a pipe with a name.
///
/// Cargo hands a token to every compiler it runs at once, and makes the
/// pipe for them itself unless it is given one. Its own has no name: the
/// compilers inherit it, and to have them inherit it Cargo starts each as
/// a copy of itself that then becomes the compiler. The copy is of all
/// the memory Cargo has in use for the workspace, made and dropped again
/// for every compiler, one at a time. A build that has two thousand
/// compilers to start and an answer ready for each spends most of its
/// time there. A pipe with a name is found by that name, nothing has to
/// be inherited, and Cargo starts a compiler without a copy of itself.
///
/// The number of tokens is the one Cargo would have chosen, and only
/// where nothing says another: no option, variable or configuration file.
/// A make older than 4.4 stops at a pipe with a name, so none is given
/// where a build script would find such a make.
pub fn offer(cmd: &mut Command, cache: &Path) -> Option<Tokens> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    let asked = std::env::var_os("JATO_JOBSERVER");
    if asked.as_deref() == Some(OsStr::new("0")) {
        return None;
    }
    let jobs = std::thread::available_parallelism().ok()?.get();
    if jobs < 2 || says_how_many(cmd) {
        return None;
    }
    if asked.as_deref() != Some(OsStr::new("1")) && !every_make_takes_a_name() {
        return None;
    }
    let dir = cache.join("locks").join("jobs");
    // The name is read up to the first space.
    if dir
        .as_os_str()
        .as_bytes()
        .iter()
        .any(u8::is_ascii_whitespace)
    {
        return None;
    }
    fs::create_dir_all(&dir).ok()?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let path = dir.join(format!("tokens-{}-{nanos:x}", std::process::id()));
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } != 0 {
        return None;
    }
    let tokens = (|| {
        use std::io::Write;
        let mut pipe = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .ok()?;
        // Cargo holds one itself, as make's rules have it.
        pipe.write_all(&vec![b'|'; jobs - 1]).ok()?;
        Some(Tokens {
            path: path.clone(),
            _pipe: pipe,
        })
    })();
    if tokens.is_none() {
        let _ = fs::remove_file(&path);
        return None;
    }
    let mut said = OsString::from(format!("-j{jobs} --jobserver-auth=fifo:"));
    said.push(&path);
    cmd.env(TOLD_IN[0], said);
    tokens
}

/// Take back what `offer` told a command that is started without it.
pub fn withdraw(cmd: &mut Command, tokens: Option<Tokens>) {
    if tokens.is_some() {
        cmd.env_remove(TOLD_IN[0]);
    }
}

/// Whether anything tells the Cargo that `cmd` starts how many compilers
/// to run at once, or where its tokens are. Anything that might is taken
/// to: the setting is the person's.
fn says_how_many(cmd: &Command) -> bool {
    let set = |name: &str| {
        cmd.get_envs()
            .find(|(known, _)| *known == OsStr::new(name))
            .map_or_else(
                || std::env::var_os(name).is_some(),
                |(_, value)| value.is_some(),
            )
    };
    if set("CARGO_BUILD_JOBS") || TOLD_IN.iter().any(|name| set(name)) {
        return true;
    }
    let args: Vec<&OsStr> = cmd.get_args().take_while(|arg| *arg != "--").collect();
    if args_say_how_many(&args) {
        return true;
    }
    let cwd = match cmd.get_current_dir() {
        Some(dir) => dir.to_owned(),
        None => match std::env::current_dir() {
            Ok(dir) => dir,
            Err(_) => return true,
        },
    };
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".cargo")));
    let dirs = cwd.ancestors().map(|dir| dir.join(".cargo")).chain(home);
    for dir in dirs {
        for file in ["config.toml", "config"] {
            match fs::read(dir.join(file)) {
                Ok(text) if settings_say_how_many(&text) => return true,
                Ok(_) => (),
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                // Not a directory, for one: then it is no setting either.
                Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) => (),
                Err(_) => return true,
            }
        }
    }
    false
}

/// Whether Cargo's arguments say how many, or might: by `-j`, by a
/// setting given on the command line, or from another directory, whose
/// settings would then be the ones that count.
fn args_say_how_many(args: &[&OsStr]) -> bool {
    use std::os::unix::ffi::OsStrExt;
    args.iter().any(|arg| {
        let arg = arg.as_bytes();
        arg.starts_with(b"-j")
            || arg.starts_with(b"--jobs")
            || arg.starts_with(b"--config")
            || arg == b"-C"
            || arg.starts_with(b"-Z")
    })
}

/// Whether a configuration file of Cargo's says how many, or might: a
/// line that is no comment and has `jobs` in it, which is also how an
/// alias would say it in full, or `-j`, or that takes in other files.
fn settings_say_how_many(text: &[u8]) -> bool {
    text.split(|byte| *byte == b'\n').any(|line| {
        let line = line.trim_ascii_start();
        !line.starts_with(b"#")
            && [&b"jobs"[..], b"-j", b"include"]
                .iter()
                .any(|word| memchr::memmem::find(line, word).is_some())
    })
}

/// Whether each `make` a build script would find along PATH is one that
/// takes tokens from a pipe with a name. With none there, none will be
/// started by that name.
fn every_make_takes_a_name() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    ["make", "gmake"].iter().all(|name| {
        let found = std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|program| program.is_file());
        let Some(program) = found else {
            return true;
        };
        Command::new(program)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .is_ok_and(|said| said.status.success() && make_takes_a_name(&said.stdout))
    })
}

/// Whether a make that says this of its version is GNU make 4.4 or later.
fn make_takes_a_name(said: &[u8]) -> bool {
    let line = said.split(|byte| *byte == b'\n').next().unwrap_or_default();
    let Some(version) = line.strip_prefix(b"GNU Make ") else {
        return false;
    };
    let mut numbers = version
        .split(|byte| !byte.is_ascii_digit())
        .map(|digits| std::str::from_utf8(digits).ok()?.parse::<u32>().ok());
    match (numbers.next().flatten(), numbers.next().flatten()) {
        (Some(major), Some(minor)) => (major, minor) >= (4, 4),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn a_pipe_with_a_name_is_made_out_where_a_build_is_told_of_one() {
        let pipe = |said: &str| named_pipe(OsStr::new(said));
        assert_eq!(
            pipe("-j8 --jobserver-auth=fifo:/run/user/1/tokens-7"),
            Some(PathBuf::from("/run/user/1/tokens-7"))
        );
        assert_eq!(
            pipe("-j --jobserver-fds=fifo:/old --jobserver-auth=fifo:/new -- X=1"),
            Some(PathBuf::from("/new"))
        );
        assert_eq!(pipe("-j --jobserver-fds=8,9 --jobserver-auth=8,9"), None);
        assert_eq!(pipe("--jobserver-auth=fifo:"), None);
        assert_eq!(pipe(""), None);
    }

    #[test]
    fn what_says_how_many_compilers_to_run_is_left_to_say_it() {
        let args = |line: &str| {
            let words: Vec<&OsStr> = line.split(' ').map(OsStr::new).collect();
            args_say_how_many(&words)
        };
        assert!(!args("build --release -p app"));
        assert!(!args("+nightly check --workspace --message-format=json"));
        for line in [
            "build -j4",
            "build -j 4",
            "check --jobs=2",
            "check --jobs 2",
            "build --config build.jobs=3",
            "build --config=other.toml",
            "-C elsewhere build",
            "build -Zunstable-options",
        ] {
            assert!(args(line), "{line}");
        }
        let settings = |text: &str| settings_say_how_many(text.as_bytes());
        assert!(!settings(
            "# jobs = 2 would be too few\n[build]\nrustflags = [\"-Clink-arg=-fuse-ld=mold\"]\n\n[env]\nCC = \"cc\"\n"
        ));
        assert!(settings("[build]\njobs = 4\n"));
        assert!(settings("build.jobs = \"default\"\n"));
        assert!(settings("[alias]\nb = \"build -j2\"\n"));
        assert!(settings("  include = \"more.toml\"\n"));
        assert!(!settings(""));
    }

    #[test]
    fn a_make_is_asked_its_version_before_it_is_given_a_name() {
        let takes = |said: &str| make_takes_a_name(said.as_bytes());
        assert!(takes("GNU Make 4.4.1\nBuilt for x86_64-pc-linux-gnu\n"));
        assert!(takes("GNU Make 4.4\n"));
        assert!(takes("GNU Make 5.0\n"));
        assert!(!takes("GNU Make 4.3\nBuilt for x86_64-pc-linux-gnu\n"));
        assert!(!takes("GNU Make 3.81\n"));
        assert!(!takes("bmake 20240108\n"));
        assert!(!takes(""));
    }

    #[test]
    fn no_more_than_the_limit_hold_a_place_and_the_next_gets_one_freed() {
        let dir = tempfile::tempdir().unwrap();
        let first = take_one_of(dir.path(), 2);
        let second = take_one_of(dir.path(), 2);
        assert!(first.file.is_some() && second.file.is_some());
        let (sent, got) = mpsc::channel();
        let cache = dir.path().to_owned();
        let waiter = std::thread::spawn(move || {
            let third = take_one_of(&cache, 2);
            sent.send(third.waited).unwrap();
            third
        });
        assert!(got.recv_timeout(Duration::from_millis(150)).is_err());
        drop(first);
        let waited = got.recv_timeout(Duration::from_secs(20)).unwrap();
        assert!(waited >= Duration::from_millis(100), "{waited:?}");
        let third = waiter.join().unwrap();
        assert!(third.file.is_some());
        // No limit, and no directory to keep locks in, both mean go ahead.
        assert!(take_one_of(dir.path(), 0).file.is_none());
        assert!(take_one_of(Path::new("/proc/nowhere"), 2).file.is_none());
        drop((second, third));
        assert!(take_one_of(dir.path(), 1).file.is_some());
    }
}
