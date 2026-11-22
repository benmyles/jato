mod cas;
mod command;
mod dev;
mod keep;
mod manifest;
mod observe;
mod roots;
mod scan;
mod watch;

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
  jato --keep-build build [...]    Retain Cargo intermediates when changing target directories
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
    keep_build: bool,
}

fn cargo_command(args: &[OsString], options: CargoOptions) -> Result<Command> {
    let mut cmd = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    if options.dev {
        dev::args(&mut cmd, args);
    } else {
        cmd.args(args);
    }
    cmd.env("RUSTC_WRAPPER", env::current_exe()?);
    if options.keep_build {
        keep::configure(&mut cmd);
    }
    if env::var_os("JATO_CACHE_DIR").is_some() {
        cmd.env("JATO_CACHE_DIR", cache_dir()?);
    }
    Ok(cmd)
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
            Some("--keep-build") => options.keep_build = true,
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
            return command::passthrough(cargo_command(&build, options)?);
        }
    }
    let first = args.first().map(OsString::as_os_str);
    match first.and_then(OsStr::to_str) {
        Some("--version" | "-V") => println!("jato {}", env!("CARGO_PKG_VERSION")),
        Some("--help" | "-h" | "help") => print!("{HELP}"),
        Some("check" | "build" | "test" | "run" | "clippy") => {
            return command::passthrough(cargo_command(args, options)?);
        }
        Some("watch") => return watch::run(&args[1..], options),
        Some("cargo") => return command::passthrough(cargo_command(&args[1..], options)?),
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
