mod admission;
mod compiler;
mod fingerprint;
mod invocation;
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

fn cargo_command(args: &[OsString]) -> Result<Command> {
    let mut cmd = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.args(args).env("RUSTC_WRAPPER", env::current_exe()?);
    if env::var_os("JATO_CACHE_DIR").is_some() {
        cmd.env("JATO_CACHE_DIR", cache_dir()?);
    }
    Ok(cmd)
}

fn run() -> Result<i32> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    let first = args.first().map(OsString::as_os_str);
    let verb = first.and_then(OsStr::to_str);
    if let Some(driver) = first
        && (Path::new(driver).components().count() > 1
            || driver == "rustc"
            || env::var_os("RUSTC_WORKSPACE_WRAPPER").as_deref() == Some(driver)
            || env::var_os("RUSTC").as_deref() == Some(driver)
            || (env::var_os("CARGO").is_some()
                && args.get(1).is_some_and(|s| {
                    s == "-vV"
                        || s == "--crate-name"
                        || Path::new(s).file_name() == Some(OsStr::new("rustc"))
                })))
    {
        return compiler::wrap(driver, &args[1..]);
    }
    match verb {
        Some("--version" | "-V") => println!("jato {}", env!("CARGO_PKG_VERSION")),
        Some("--help" | "-h" | "help") | None => {
            print!("{HELP}")
        }
        Some("check" | "build" | "test" | "run" | "clippy") => {
            return compiler::passthrough(cargo_command(&args)?);
        }
        Some("watch") => return watch::run(&args[1..]),
        Some("cargo") => return compiler::passthrough(cargo_command(&args[1..])?),
        Some("stats") if args.len() == 1 || args[1..] == ["--json"] => {
            let stats = Store::open()?.stats()?;
            if args.len() == 2 {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                println!("Cache     {}", stats.directory.display());
                println!(
                    "Entries   {} ({} unique chunks)",
                    stats.entries, stats.chunks
                );
                println!(
                    "Storage   {} charged bytes / {} budget",
                    stats.charged_bytes, stats.limit_bytes
                );
                println!(
                    "Payload   {} compressed bytes for {} artifact bytes",
                    stats.compressed_bytes, stats.artifact_bytes
                );
                for (name, value) in stats.counters {
                    println!("{name:<20} {value}");
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
        _ => bail!("unknown command or arguments. Run jato --help"),
    }
    Ok(0)
}
