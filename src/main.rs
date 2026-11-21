mod admission;
mod compiler;
mod content;
mod fingerprint;
mod invocation;
mod native;
mod probe;
mod sealed;
mod staging;
mod store;
mod watch;

use anyhow::{Context, Result, bail};
use std::env;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;
use store::{Store, cache_dir, cache_limit, parse_size};

const HELP: &str = "\
Jato — a local Rust build cache and workspace edit runner

Usage
  jato check|build|test|run|clippy [CARGO_ARGUMENTS...]
  jato cargo [ARGUMENTS...]        Run any Cargo command, including +toolchain
  jato watch [OPTIONS]             Check on save, with changed packages first
  jato stats [--json]              Show cache contents and compiler counters
  jato gc [SIZE]                   Evict least-recently-used entries to SIZE
  jato --system-linker build [...]   Select /usr/bin/cc for x86_64 GNU host links
  jato --version

Examples
  jato check --all-targets
  jato test -p parser -- --nocapture
  jato cargo +stable build --release
  jato gc 256MiB

Environment
  JATO_CACHE_DIR    Private local cache, default $XDG_CACHE_HOME/jato
  JATO_MAX_SIZE     Persistent cache budget, default 1GiB
  JATO_LOG=debug    Explain each cache decision on stderr
  JATO_LOG=timings  Include the time spent validating and restoring a hit
  JATO_STORAGE=packed  Use compressed chunks for all new artifacts
  JATO_LINK_CACHE=0  Delegate native linking directly to rustc
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

fn cargo_command(args: &[OsString], system_linker: bool) -> Result<Command> {
    let mut cmd = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.args(args).env("RUSTC_WRAPPER", env::current_exe()?);
    if system_linker {
        anyhow::ensure!(
            cfg!(all(
                target_arch = "x86_64",
                target_os = "linux",
                target_env = "gnu"
            )) && Path::new("/usr/bin/cc").is_file(),
            "--system-linker needs the x86_64 Linux GNU system C driver"
        );
        cmd.env(
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER",
            "/usr/bin/cc",
        );
    }
    if env::var_os("JATO_CACHE_DIR").is_some() {
        cmd.env("JATO_CACHE_DIR", cache_dir()?);
    }
    Ok(cmd)
}

fn run() -> Result<i32> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    let system_linker = args.first().is_some_and(|a| a == "--system-linker");
    let args = if system_linker { &args[1..] } else { &args[..] };
    let first = args.first().map(OsString::as_os_str);
    let verb = first.and_then(OsStr::to_str);
    match verb {
        Some("--version" | "-V") => println!("jato {}", env!("CARGO_PKG_VERSION")),
        Some("--help" | "-h" | "help") => {
            print!("{HELP}")
        }
        Some("check" | "build" | "test" | "run" | "clippy") => {
            return compiler::passthrough(cargo_command(args, system_linker)?);
        }
        Some("watch") => return watch::run(&args[1..], system_linker),
        Some("cargo") => return compiler::passthrough(cargo_command(&args[1..], system_linker)?),
        Some("stats") if args.len() == 1 || args[1..] == ["--json"] => {
            let stats = Store::open()?.stats()?;
            if args.len() == 2 {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                let hits = stats.counters.get("hit").copied().unwrap_or(0);
                let misses = stats.counters.get("miss").copied().unwrap_or(0);
                println!("Cache     {}", stats.directory.display());
                println!("Lookups   {hits} hits, {misses} compiler runs after lookup");
                println!(
                    "Database  {:.2} MiB across SQLite files",
                    stats.database_bytes as f64 / 1048576.0
                );
                println!(
                    "Entries   {} ({} unique chunks)",
                    stats.entries, stats.chunks
                );
                println!(
                    "Storage   {} charged bytes / {} budget",
                    stats.charged_bytes, stats.limit_bytes
                );
                println!(
                    "Payload   {} packed bytes + {} sealed bytes for {} artifact bytes",
                    stats.compressed_bytes, stats.sealed_bytes, stats.artifact_bytes
                );
                for (name, value) in stats.counters {
                    if name != "hit" && name != "miss" {
                        println!("{name:<20} {value}");
                    }
                }
            }
        }
        Some("gc") if args.len() <= 2 => {
            let limit = args
                .get(1)
                .map(|a| parse_size(a.to_str().context("size must be UTF-8")?))
                .transpose()?
                .unwrap_or(cache_limit()?);
            println!("Evicted {} entries", Store::open()?.prune(limit)?);
        }
        None if first.is_none() => print!("{HELP}"),
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
            return compiler::wrap(first.unwrap(), &args[1..]);
        }
        _ => bail!("unknown command or arguments. Run jato --help"),
    }
    Ok(0)
}
