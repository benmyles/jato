mod cas;
mod command;
mod dev;
mod facts;
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
Jato — a local Rust build cache and workspace edit runner

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

To use Jato for every Cargo command, name it in ~/.cargo/config.toml:
  [build]
  rustc-wrapper = \"jato\"

Environment
  JATO_CACHE_DIR    Private local cache, default $XDG_CACHE_HOME/jato
  JATO_MAX_SIZE     Cache budget when none is stored with `jato limit`
  JATO_LOG=debug    Explain each decision on stderr; an absolute path appends to that file
  JATO_KEY_ENV      Comma-separated variables a proc macro reads from the environment
  JATO_COALESCE=0   Let identical concurrent compilations run independently
  JATO_REMAP=0      Leave real workspace and target paths in compiled output
  JATO_DISABLE=1    Run the compiler directly
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

/// Run Cargo with Jato as its compiler wrapper. A command that only builds
/// is looked up as a whole first, and recorded as a whole if it runs. One
/// that goes on to run what it built has its build filled in when that is
/// known, and is otherwise left to Cargo.
fn cargo(args: &[OsString], options: CargoOptions) -> Result<i32> {
    let cmd = cargo_command(args, options)?;
    let end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let flag = |name: &str| args[..end].iter().any(|a| a == name);
    match subcommand(args) {
        Some((_, "build" | "b" | "check" | "c" | "clippy")) => session::build(cmd, "cargo"),
        Some((_, "test" | "t" | "bench")) if flag("--no-run") => session::build(cmd, "cargo"),
        Some((at, verb @ ("test" | "t" | "bench" | "run" | "r"))) => {
            // What this command builds before it runs anything.
            let mut build: Vec<OsString> = args[..end].to_vec();
            if matches!(verb, "run" | "r") {
                build[at] = "build".into();
            } else {
                build.insert(at + 1, "--no-run".into());
            }
            session::restore(&cargo_command(&build, options)?, "cargo");
            command::passthrough(cmd)
        }
        _ => command::passthrough(cmd),
    }
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / 1048576.0)
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
            print!("{}", dev::HELP);
            return Ok(0);
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
        Some("--version" | "-V") => println!("jato {}", env!("CARGO_PKG_VERSION")),
        Some("--help" | "-h" | "help") => print!("{HELP}"),
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
                println!("{}", serde_json::to_string_pretty(&stats)?);
                return Ok(0);
            }
            let count = |name: &str| stats.counters.get(name).copied().unwrap_or(0);
            let (hits, misses) = (count("hit"), count("miss"));
            println!("Cache      {}", stats.directory.display());
            println!(
                "Commands   {hits} reused, {misses} run ({} of those could not be stored)",
                count("uncacheable")
            );
            if hits + misses > 0 {
                println!(
                    "Reuse      {:.1}% of commands, {:.1} s of compiler time not spent",
                    100.0 * hits as f64 / (hits + misses) as f64,
                    count("saved_ms") as f64 / 1000.0
                );
            }
            println!(
                "Results    {} stored, {} distinct files",
                stats.entries, stats.objects
            );
            println!(
                "Disk       {} of {} budget, holding {} of outputs",
                mib(stats.stored_bytes),
                mib(stats.limit_bytes),
                mib(stats.artifact_bytes)
            );
            println!(
                "Index      {}, {} file digests remembered",
                mib(stats.database_bytes),
                stats.remembered_files
            );
            for (name, value) in &stats.counters {
                if !matches!(
                    name.as_str(),
                    "hit" | "miss" | "uncacheable" | "saved_ms" | "stored"
                ) {
                    println!("{name:<18} {value}");
                }
            }
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
        None => print!("{HELP}"),
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
