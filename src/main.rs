mod cas;
mod command;
mod dev;
mod facts;
mod jobs;
mod manifest;
mod observe;
mod roots;
mod scan;
#[cfg(target_os = "linux")]
mod session;
#[cfg(not(target_os = "linux"))]
mod session {
    //! Whole builds are only recorded where commands can be observed.
    use anyhow::Result;
    use std::ffi::{OsStr, OsString};
    pub fn ask(_: &OsStr, _: &OsStr, _: &[OsString]) -> Result<Option<i32>> {
        Ok(None)
    }
    pub fn build(cmd: std::process::Command, _: &str) -> Result<i32> {
        crate::command::passthrough(cmd)
    }
    pub fn ready(_: std::process::Command, _: &str) {}
}
mod watch;
mod wire;

use anyhow::{Context, Result, bail};
use cas::{Store, cache_dir, parse_size};
use std::env;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;

const HELP: &str = "\
Jato — remembers Rust builds, so the same build is never done twice

Usage
  jato check|build|test|run|clippy [CARGO_ARGUMENTS...]
  jato cargo [ARGUMENTS...]        Run any Cargo command, including +toolchain
  jato dev [ACTION] [ARGS...]       Use the fast development profile (default build)
  jato watch [OPTIONS]             Check on save, with changed packages first
  jato stats [--json]              Show cache contents and counters
  jato limit [SIZE|--unset]         Read or set this cache directory's budget
  jato gc [SIZE]                   Evict least-recently-used results to SIZE
  jato --version

Examples
  jato check --all-targets
  jato test -p parser -- --nocapture
  jato cargo +stable build --release
  jato gc 20GiB

For tools that start Cargo themselves, name Jato as the compiler wrapper in
~/.cargo/config.toml. Single compilations are then reused; whole builds are
recorded and restored only by the commands above.
  [build]
  rustc-wrapper = \"jato\"

Environment
  JATO_CACHE_DIR      Private local cache, default $XDG_CACHE_HOME/jato
  JATO_MAX_SIZE       Cache budget when none is stored with `jato limit`
  JATO_JOBS           Compilers running at once across all builds using this
                      cache; default is the number of processors, 0 any number
  JATO_LOG=debug      Explain each decision on stderr; an absolute path
                      appends to that file
  JATO_KEY_ENV        Comma-separated variables a proc macro reads from the
                      environment
  JATO_EDIT_WINDOW    Seconds within which a crate compiled again counts as
                      being edited and is not recorded; default 600, 0 never
  JATO_WHOLE_BUILDS=0 Reuse single compilations only
  JATO_COALESCE=0     Let identical concurrent compilations run independently
  JATO_REMAP=0        Leave real workspace and target paths in compiled output
  JATO_STORAGE=packed Keep compressed copies even where files can be cloned
  JATO_VERIFY=1       Check restored bytes on filesystems that checksum them
  JATO_DISABLE=1      Run Cargo and the compiler directly
";

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("jato: {e:#}");
            std::process::exit(1);
        }
    }
}

#[derive(Clone, Copy, Default)]
struct CargoOptions {
    dev: bool,
}

fn cargo_command(args: &[OsString], options: CargoOptions) -> Result<Command> {
    let mut cmd = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    if options.dev {
        dev::args(&mut cmd, args);
    } else {
        cmd.args(args);
    }
    cmd.env("RUSTC_WRAPPER", env::current_exe()?);
    if env::var_os("JATO_CACHE_DIR").is_some() {
        cmd.env("JATO_CACHE_DIR", cache_dir()?);
    }
    Ok(cmd)
}

/// The Cargo subcommand among `args`, and where it stands.
fn subcommand(args: &[OsString]) -> Option<(usize, &str)> {
    let end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let mut i = 0;
    while i < end {
        match args[i].to_str() {
            Some("--config" | "--color" | "-Z" | "-C" | "--explain") => i += 1,
            Some(word) if word.starts_with('-') || word.starts_with('+') => (),
            Some(word) => return Some((i, word)),
            None => return None,
        }
        i += 1;
    }
    None
}

/// Options of Cargo's building commands that are followed by their value.
const VALUED: &[&str] = &[
    "-p",
    "--package",
    "--exclude",
    "-j",
    "--jobs",
    "--bin",
    "--example",
    "--test",
    "--bench",
    "-F",
    "--features",
    "--target",
    "--target-dir",
    "--artifact-dir",
    "--profile",
    "--manifest-path",
    "--lockfile-path",
    "--message-format",
    "--color",
    "--config",
    "-Z",
];

/// Options of the same commands that stand alone.
const ALONE: &[&str] = &[
    "--lib",
    "--bins",
    "--examples",
    "--tests",
    "--benches",
    "--all-targets",
    "--workspace",
    "--all",
    "--all-features",
    "--no-default-features",
    "--release",
    "-r",
    "--no-run",
    "--no-fail-fast",
    "--keep-going",
    "--ignore-rust-version",
    "--locked",
    "--offline",
    "--frozen",
    "--future-incompat-report",
    "--timings",
    "-v",
    "-vv",
    "--verbose",
    "-q",
    "--quiet",
];

/// The build a `test`, `bench` or `run` command makes before it runs
/// anything, as a command of its own. `at` is where the subcommand stands.
/// A name to filter tests by and the arguments of the program to run do not
/// change what is built, so they are left out; a word that might be the
/// value of an option this does not know is kept. None if the command has
/// no build that can be asked for alone.
fn build_only(args: &[OsString], at: usize) -> Option<Vec<OsString>> {
    let end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let run = matches!(args[at].to_str(), Some("run" | "r"));
    let mut build: Vec<OsString> = args[..at].to_vec();
    build.push(if run {
        "build".into()
    } else {
        args[at].clone()
    });
    let mut unknown = false;
    let mut named = false;
    let mut i = at + 1;
    while i < end {
        let arg = &args[i];
        let text = arg.to_str()?;
        if text == "--doc" {
            // Documentation tests are compiled as they are run.
            return None;
        }
        if text.starts_with('-') {
            build.push(arg.clone());
            if VALUED.contains(&text) {
                build.push(args.get(i + 1)?.clone());
                i += 1;
                unknown = false;
            } else {
                unknown = !(ALONE.contains(&text)
                    || text.contains('=')
                    || VALUED
                        .iter()
                        .any(|flag| flag.len() == 2 && text.starts_with(flag)));
            }
        } else if unknown {
            build.push(arg.clone());
            unknown = false;
        } else if run {
            // The program's arguments start here.
            break;
        } else if named {
            return None;
        } else {
            named = true;
        }
        i += 1;
    }
    if !run {
        build.push("--no-run".into());
    }
    Some(build)
}

/// Run Cargo with Jato as its compiler wrapper. A command that only builds
/// is looked up as a whole first, and recorded as a whole if it runs. One
/// that goes on to run what it built has its build filled in or made first,
/// so that it too is recorded as a whole, and is then left to Cargo.
fn cargo(args: &[OsString], options: CargoOptions) -> Result<i32> {
    let cmd = cargo_command(args, options)?;
    let end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let flag = |name: &str| args[..end].iter().any(|a| a == name);
    match subcommand(args) {
        Some((_, "build" | "b" | "check" | "c" | "clippy")) => session::build(cmd, "cargo"),
        Some((_, "test" | "t" | "bench")) if flag("--no-run") => session::build(cmd, "cargo"),
        Some((at, "test" | "t" | "bench" | "run" | "r")) => {
            if let Some(build) = build_only(args, at) {
                session::ready(cargo_command(&build, options)?, "cargo");
            }
            command::passthrough(cmd)
        }
        _ => command::passthrough(cmd),
    }
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / 1048576.0)
}

/// Print, and end quietly if whoever was reading has stopped.
fn say(text: &str) -> Result<i32> {
    use std::io::Write;
    match std::io::stdout().write_all(text.as_bytes()) {
        Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => Err(e.into()),
        _ => Ok(0),
    }
}

/// What the cache holds and what it has done, for a person to read.
fn report(stats: &cas::Stats) -> String {
    use std::fmt::Write;
    let count = |name: &str| stats.counters.get(name).copied().unwrap_or(0);
    let mut out = String::new();
    let _ = writeln!(out, "Cache      {}", stats.directory.display());
    let (whole, made, left) = (
        count("build_hit"),
        count("build_miss"),
        count("build_left_to_cargo"),
    );
    if whole + made + left > 0 {
        let _ = writeln!(
            out,
            "Builds     {whole} put back whole, {made} made and {} of those recorded, {left} left to Cargo in a built target",
            count("build_stored")
        );
    }
    let (hits, misses, edits) = (count("hit"), count("miss"), count("continued"));
    let _ = writeln!(
        out,
        "Compilers  {hits} results reused, {misses} run ({} of those could not be stored), {edits} run during edits",
        count("uncacheable")
    );
    let _ = writeln!(
        out,
        "Saved      {:.1} s of earlier work not done again",
        count("saved_ms") as f64 / 1000.0
    );
    if count("queued") > 0 {
        let _ = writeln!(
            out,
            "Waiting    {} compilers waited {:.1} s in all for other builds",
            count("queued"),
            count("queued_ms") as f64 / 1000.0
        );
    }
    let _ = writeln!(
        out,
        "Results    {} stored, {} distinct files",
        stats.entries, stats.objects
    );
    let _ = writeln!(
        out,
        "Disk       {} of {} budget, holding {} of outputs",
        mib(stats.stored_bytes),
        mib(stats.limit_bytes),
        mib(stats.artifact_bytes)
    );
    let _ = writeln!(
        out,
        "Index      {}, {} file digests remembered",
        mib(stats.database_bytes),
        stats.remembered_files
    );
    const SHOWN: &[&str] = &[
        "hit",
        "miss",
        "uncacheable",
        "saved_ms",
        "stored",
        "continued",
        "queued",
        "queued_ms",
        "build_hit",
        "build_miss",
        "build_stored",
        "build_left_to_cargo",
    ];
    for (name, value) in &stats.counters {
        if !SHOWN.contains(&name.as_str()) {
            let _ = writeln!(out, "{name:<18} {value}");
        }
    }
    out
}

fn run() -> Result<i32> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    let mut options = CargoOptions::default();
    let mut args = &args[..];
    while let Some(flag) = args.first() {
        match flag.to_str() {
            Some("dev") => options.dev = true,
            _ => break,
        }
        args = &args[1..];
    }
    if options.dev {
        if matches!(
            args.first().and_then(|a| a.to_str()),
            Some("-h" | "--help" | "help")
        ) {
            return say(dev::HELP);
        }
        if args.is_empty()
            || (args[0].as_encoded_bytes().starts_with(b"-")
                && args[0] != "--version"
                && args[0] != "-V")
        {
            let mut build = vec![OsString::from("build")];
            build.extend_from_slice(args);
            return cargo(&build, options);
        }
    }
    let first = args.first().map(OsString::as_os_str);
    match first.and_then(OsStr::to_str) {
        Some("--version" | "-V") => {
            return say(concat!("jato ", env!("CARGO_PKG_VERSION"), "\n"));
        }
        Some("--help" | "-h" | "help") => return say(HELP),
        Some("check" | "build" | "test" | "run" | "clippy") => return cargo(args, options),
        Some("watch") => return watch::run(&args[1..], options),
        Some("cargo") => return cargo(&args[1..], options),
        // Started by this program to see a command's stragglers out.
        Some("__answer") if args.len() == 2 => {
            let fd = args[1].to_str().and_then(|s| s.parse().ok());
            return Ok(observe::answer(fd.context("bad descriptor")?));
        }
        Some("stats") if args.len() == 1 || args[1..] == ["--json"] => {
            let stats = Store::open()?.stats()?;
            if args.len() == 2 {
                return say(&format!("{}\n", serde_json::to_string_pretty(&stats)?));
            }
            return say(&report(&stats));
        }
        Some("limit") if args.len() <= 2 => {
            let mut store = Store::open()?;
            let size = match args.get(1) {
                None => store.budget()?,
                Some(arg) if arg == "--unset" => store.set_budget(None)?,
                Some(arg) => store.set_budget(Some(parse_size(
                    arg.to_str().context("size must be UTF-8")?,
                )?))?,
            };
            println!("{size} bytes");
        }
        Some("gc") if args.len() <= 2 => {
            let limit = args
                .get(1)
                .map(|a| parse_size(a.to_str().context("size must be UTF-8")?))
                .transpose()?;
            let (evicted, earlier) = Store::open()?.prune(limit)?;
            println!("Evicted {evicted} results");
            if earlier > 0 {
                println!("Removed {} left by earlier versions of Jato", mib(earlier));
            }
        }
        None => return say(HELP),
        // Cargo runs its compiler wrapper as `jato <compiler> <arguments>`.
        _ if first.is_some_and(|driver| {
            Path::new(driver).components().count() > 1
                || driver == "rustc"
                || env::var_os("RUSTC_WORKSPACE_WRAPPER").as_deref() == Some(driver)
                || env::var_os("RUSTC").as_deref() == Some(driver)
                || (env::var_os("CARGO").is_some()
                    && args[1..].iter().any(|a| {
                        a == "-vV"
                            || a == "--crate-name"
                            || a == "--print"
                            || a.as_encoded_bytes().starts_with(b"--print=")
                            || Path::new(a).file_name() == Some(OsStr::new("rustc"))
                    }))
        }) =>
        {
            return command::wrap(first.unwrap(), &args[1..]);
        }
        _ => bail!("unknown command or arguments. Run jato --help"),
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<OsString> {
        text.split(' ').map(OsString::from).collect()
    }

    #[test]
    fn the_build_behind_a_test_or_a_program_is_asked_for_alone() {
        let build = |text: &str| {
            let args = words(text);
            let at = subcommand(&args).unwrap().0;
            build_only(&args, at).map(|b| {
                b.iter()
                    .map(|a| a.to_str().unwrap())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
        };
        let is = |text: &str| Some(text.to_owned());
        assert_eq!(build("test"), is("test --no-run"));
        assert_eq!(
            build("test -p leaf parses_ --release -- --nocapture x"),
            is("test -p leaf --release --no-run")
        );
        assert_eq!(build("+nightly t -j4 name"), is("+nightly t -j4 --no-run"));
        assert_eq!(
            build("bench --bench speed"),
            is("bench --bench speed --no-run")
        );
        assert_eq!(
            build("run --bin app --features=a,b one --two -- three"),
            is("build --bin app --features=a,b")
        );
        assert_eq!(build("r -q"), is("build -q"));
        // A word after an option this does not know may be its value.
        assert_eq!(
            build("test --strange word name"),
            is("test --strange word --no-run")
        );
        // Not one build: documentation tests, and a command Cargo rejects.
        assert_eq!(build("test --doc"), None);
        assert_eq!(build("test one two"), None);
        assert_eq!(build("test -p"), None);
    }
}
