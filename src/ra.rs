//! Shared `rust-analyzer` LSP multiplexer and transparent shim.
//!
//! Coding agents and editors often launch one `rust-analyzer` process per
//! session or git worktree, multiplying memory use by 20x+ because every
//! process loads `sysroot`, hundreds of dependency crates, and proc-macro
//! servers independently and keeps them resident forever while idle.
//!
//! `jato rust-analyzer` (and the optional `rust-analyzer` PATH shim installed
//! with `jato rust-analyzer shim install`) fixes this transparently:
//! - Clients in the same workspace or in sibling git worktrees of the same
//!   repository share one `rust-analyzer` backend (grouped by git common dir
//!   and toolchain), multiplexing LSP request IDs, document open/change/close
//!   refcounts, and worktree-scoped URI filtering (`publishDiagnostics`,
//!   `workspace/symbol`, `references`, `definition`, etc.).
//! - Backends start lazily on first document open or semantic query (0 B RAM
//!   while a session has only sent `initialize`), hibernate transparently when
//!   idle (preserving client connections and open document buffers, waking
//!   seamlessly on the next request), and respect a total RSS budget with LRU
//!   eviction.
//! - Each shared backend applies memory-conscious defaults (`numThreads`,
//!   `cachePriming.enable = false`, `procMacro.processes = 1`, `lru.capacity`,
//!   `MALLOC_ARENA_MAX = 2`, and a shared per-repo `cargo.targetDir` outside
//!   worktrees) while preserving explicit client settings and passing non-LSP
//!   CLI invocations straight through to the real `rust-analyzer`.

use crate::cas::{cache_dir, parse_size};
use crate::roots;
use crate::wire;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

#[path = "ra_cli.rs"]
mod cli;

pub use cli::{cli_def, cli_diag, cli_hover, cli_refs, cli_symbols};

pub const HELP: &str = "\
Usage
  jato rust-analyzer [LSP_FLAGS...]           Run as an LSP proxy sharing backends
  jato rust-analyzer status [--json]          Show daemon, shared backends, and RSS
  jato rust-analyzer stop                     Stop the shared daemon and all backends
  jato rust-analyzer shim install [--dir DIR] Install a transparent rust-analyzer shim
  jato rust-analyzer shim uninstall [--dir DIR]
                                              Remove the shim and restore any rustup link
  jato rust-analyzer shim status              Show active rust-analyzer shim status

Instant CLI queries (reuse the same warm rust-analyzer daemon):
  jato diag [OPTIONS] [FILE...]               Instant file/workspace diagnostics
  jato def <FILE:LINE:COL> [--type|--impl]    Go to definition / type / implementation
  jato refs <FILE:LINE:COL> [--no-decl]       Find all references
  jato hover <FILE:LINE:COL>                  Show type signature and docs
  jato symbols [FILE | --query QUERY]         List document or workspace symbols

Non-LSP rust-analyzer flags and subcommands (--version, analysis-stats, parse,
etc.) pass straight through to the real rust-analyzer binary.

Environment
  JATO_RA_MEMORY          Total RSS budget across backends (default: 25% RAM, 4–32 GiB; 0 disables)
  JATO_RA_MAX_BACKENDS    Max concurrent rust-analyzer processes (default: 8; 0 unlimited)
  JATO_RA_IDLE_SECS       Idle seconds before hibernating a worktree/backend (default: 600; 0 disables)
  JATO_RA_SHARE_WORKTREES 1 (default) shares one backend across git worktrees of a repo; 0 isolates
  JATO_RA_LAZY            1 (default) defers spawning until first file open/query; 0 spawns on init
  JATO_RA_THREADS         Worker threads per backend (default: 4–8)
  JATO_RA_PRIME           0 (default) disables eager cache priming; 1 enables it
  JATO_RA_PROC_MACROS     proc-macro-srv processes per backend (default: 1)
  JATO_RA_LRU             Syntax tree LRU capacity per backend (default: 128)
  JATO_RA_TARGET_DIR      1 (default) shares per-repo cargo.targetDir; 0 disables; or custom path
  JATO_RA_CACHE           1 (default) wires RUSTC_WRAPPER=jato & JATO_CACHE_DIR into backends; 0 disables
  JATO_RA_SEED            1 (default) pre-seeds an empty shared cargo.targetDir from Jato's cache; 0 disables
  JATO_RA_CHECK_JATO      1 (default) routes checkOnSave through `jato check --message-format=json`; 0 disables
  JATO_RA_CHECK_ON_SAVE   0 or 1 to override rust-analyzer checkOnSave
  JATO_RUST_ANALYZER      Explicit path to the real rust-analyzer binary
  JATO_RA_PASSTHROUGH=1   Bypass multiplexing and exec the real rust-analyzer directly
";

const TICK_INTERVAL: Duration = Duration::from_secs(1);
const RSS_REFRESH_TICKS: u64 = 5;
const RETRY_DELAY: Duration = Duration::from_millis(300);
const MAX_CONTENT_MODIFIED_RETRIES: u8 = 12;
const WORKSPACE_READY_WAIT: Duration = Duration::from_secs(25);
const WORKSPACE_BUSY_READY_WAIT: Duration = Duration::from_secs(120);
const FOLDER_BATCH_DELAY: Duration = Duration::from_millis(60);
const TOOLCHAIN_CACHE_TTL: Duration = Duration::from_secs(60);
const ORPHAN_BACKEND_GRACE: Duration = Duration::from_secs(60);
const EMPTY_DAEMON_GRACE: Duration = Duration::from_secs(30);
const MAX_LSP_MESSAGE: usize = 64 << 20;
const MAX_LSP_HEADERS: usize = 32;
const MAX_LSP_HEADER_LINE: usize = 8192;
const MAX_CLIENT_NOTIFS: usize = 256;
const MAX_CLIENT_RESPONSES: usize = 8192;
const SYNTAX_CACHE_MAX: usize = 512;

// ---- CLI entrypoints -------------------------------------------------------

/// Invoked via symlink/binary named `rust-analyzer`. Completely transparent:
/// never intercepts Jato management subcommands.
pub fn shim_main(args: &[OsString]) -> Result<i32> {
    if passthrough_requested() || !is_lsp_invocation(args) {
        return exec_real_ra(args);
    }
    run_proxy(args)
}

/// Invoked explicitly as `jato rust-analyzer [args...]`.
pub fn main(args: &[OsString]) -> Result<i32> {
    match args.first().and_then(|a| a.to_str()) {
        Some("-h" | "--help" | "help") => {
            print!("{HELP}");
            return Ok(0);
        }
        Some("status") if args.len() == 1 || args[1..] == ["--json"] => {
            return cmd_status(args.len() == 2);
        }
        Some("stop") if args.len() == 1 => return cmd_stop(),
        Some("shim") => return cmd_shim(&args[1..]),
        Some("__daemon") if args.len() == 1 => return run_daemon(),
        _ => (),
    }
    if passthrough_requested() || !is_lsp_invocation(args) {
        return exec_real_ra(args);
    }
    run_proxy(args)
}

fn passthrough_requested() -> bool {
    env::var_os("JATO_DISABLE").is_some_and(|v| v == "1")
        || env::var_os("JATO_RA_PASSTHROUGH").is_some_and(|v| v == "1")
}

/// True when `args` only contain LSP-mode flags accepted by `rust-analyzer`
/// (no subcommands like `parse`/`analysis-stats`, no `--version`/`--help`,
/// and no `RUSTC_WRAPPER` compiler args).
fn is_lsp_invocation(args: &[OsString]) -> bool {
    let mut i = 0;
    while i < args.len() {
        let Some(s) = args[i].to_str() else {
            return false;
        };
        match s {
            "-v" | "--verbose" | "-q" | "--quiet" | "--no-log-buffering" => i += 1,
            "--log-file" if i + 1 < args.len() => i += 2,
            s if s.starts_with("--log-file=") => i += 1,
            // Repeated short flags like `-vv` or `-qq`.
            s if s.starts_with('-')
                && s.len() > 1
                && (s[1..].bytes().all(|b| b == b'v') || s[1..].bytes().all(|b| b == b'q')) =>
            {
                i += 1;
            }
            _ => return false,
        }
    }
    true
}

// ---- binary & toolchain resolution -----------------------------------------

fn is_self(path: &Path) -> bool {
    let Some(own) = own_jato_exe().and_then(|p| fs::canonicalize(&p).ok().or(Some(p))) else {
        return false;
    };
    if fs::canonicalize(path).ok().is_some_and(|p| p == own) {
        return true;
    }
    if path.components().count() == 1
        && let Some(name) = path.to_str()
        && let Some(path_env) = env::var_os("PATH")
        && let Some(found) = crate::tools::find(name, &path_env)
    {
        return fs::canonicalize(&found).ok().is_some_and(|p| p == own);
    }
    false
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn is_rustup_symlink(path: &Path) -> bool {
    fs::read_link(path).ok().is_some_and(|target| {
        target
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name == "rustup")
    })
}

fn find_rustup() -> Option<PathBuf> {
    if let Some(path) = env::var_os("PATH") {
        for dir in env::split_paths(&path) {
            let cand = dir.join("rustup");
            if is_executable_file(&cand) {
                return Some(cand);
            }
        }
    }
    let home = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| Path::new(&h).join(".cargo")))?;
    let cand = home.join("bin/rustup");
    is_executable_file(&cand).then_some(cand)
}

fn rustup_which(rustup: &Path, cwd: &Path, toolchain: Option<&str>, bin: &str) -> Option<PathBuf> {
    let mut cmd = Command::new(rustup);
    cmd.arg("which");
    // Always remove inherited RUSTUP_TOOLCHAIN so daemon startup env cannot
    // bleed across clients when `toolchain` is `None`.
    cmd.env_remove("RUSTUP_TOOLCHAIN");
    if let Some(tc) = toolchain {
        cmd.arg("--toolchain").arg(tc);
    }
    cmd.arg(bin)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let path = PathBuf::from(text.trim());
    (is_executable_file(&path) && !is_self(&path)).then_some(path)
}

/// Resolve the real `rust-analyzer` executable for `cwd`, skipping Jato's shim
/// and gracefully falling back to another installed rustup toolchain when a
/// workspace's pinned toolchain (e.g. a pinned nightly) omits `rust-analyzer`.
pub fn resolve_real_ra(cwd: &Path) -> Result<PathBuf> {
    let tc = env::var("RUSTUP_TOOLCHAIN")
        .ok()
        .filter(|s| !s.trim().is_empty());
    resolve_real_ra_for(cwd, tc.as_deref())
}

fn resolve_real_ra_for(cwd: &Path, toolchain: Option<&str>) -> Result<PathBuf> {
    if let Some(explicit) = env::var_os("JATO_RUST_ANALYZER")
        && !explicit.is_empty()
    {
        let p = PathBuf::from(&explicit);
        if is_executable_file(&p) && !is_self(&p) {
            return Ok(fs::canonicalize(&p).unwrap_or(p));
        }
        bail!("JATO_RUST_ANALYZER does not point to a valid rust-analyzer binary: {explicit:?}");
    }
    if let Some(rustup) = find_rustup() {
        if let Some(found) = rustup_which(&rustup, cwd, toolchain, "rust-analyzer") {
            return Ok(fs::canonicalize(&found).unwrap_or(found));
        }
        // Active toolchain lacks rust-analyzer (e.g., pinned nightly without the
        // rust-analyzer component): fall back to stable, then any installed toolchain.
        if let Some(found) = rustup_which(&rustup, cwd, Some("stable"), "rust-analyzer") {
            return Ok(fs::canonicalize(&found).unwrap_or(found));
        }
        if let Ok(out) = Command::new(&rustup)
            .args(["toolchain", "list"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            && out.status.success()
        {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let tc = line.split_whitespace().next().unwrap_or_default();
                if tc.is_empty() {
                    continue;
                }
                if let Some(found) = rustup_which(&rustup, cwd, Some(tc), "rust-analyzer") {
                    return Ok(fs::canonicalize(&found).unwrap_or(found));
                }
            }
        }
    }
    if let Some(path) = env::var_os("PATH") {
        for dir in env::split_paths(&path) {
            let cand = dir.join("rust-analyzer");
            if is_executable_file(&cand) && !is_self(&cand) && !is_rustup_symlink(&cand) {
                return Ok(fs::canonicalize(&cand).unwrap_or(cand));
            }
        }
    }
    bail!(
        "could not find a real `rust-analyzer` binary (install with `rustup component add rust-analyzer` or set JATO_RUST_ANALYZER)"
    )
}

fn resolve_rustc_for(cwd: &Path, toolchain: Option<&str>) -> PathBuf {
    if let Some(rustup) = find_rustup()
        && let Some(found) = rustup_which(&rustup, cwd, toolchain, "rustc")
    {
        return fs::canonicalize(&found).unwrap_or(found);
    }
    env::var_os("PATH")
        .and_then(|p| crate::tools::find("rustc", &p))
        .and_then(|p| fs::canonicalize(&p).ok())
        .unwrap_or_default()
}

fn exec_real_ra(args: &[OsString]) -> Result<i32> {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let real = resolve_real_ra(&cwd)?;
    let mut cmd = Command::new(&real);
    cmd.args(args).env("JATO_RA_PASSTHROUGH", "1");
    crate::command::passthrough(cmd)
}

// ---- shim installer --------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct ShimState {
    path: PathBuf,
    previous_symlink: Option<PathBuf>,
}

fn ra_state_dir() -> Result<PathBuf> {
    let dir = cache_dir()?.join("rust-analyzer");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn shim_state_path() -> Result<PathBuf> {
    Ok(ra_state_dir()?.join("shim.json"))
}

fn cargo_bin_dir() -> Option<PathBuf> {
    env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| Path::new(&h).join(".cargo")))
        .map(|h| h.join("bin"))
}

fn local_bin_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(|h| Path::new(&h).join(".local/bin"))
}

fn atomic_symlink(target: &Path, dest: &Path) -> Result<()> {
    let parent = dest.parent().context("destination has no parent")?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".jato-ra-shim-{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp)
        .with_context(|| format!("creating symlink {}", tmp.display()))?;
    if let Err(e) = fs::rename(&tmp, dest) {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("installing symlink at {}", dest.display()));
    }
    Ok(())
}

fn first_ra_on_path() -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let cand = dir.join("rust-analyzer");
        if fs::symlink_metadata(&cand).is_ok() && is_executable_file(&cand) {
            return Some(cand);
        }
    }
    None
}

const SHIM_HELP: &str = "\
Usage
  jato rust-analyzer shim install [--dir DIR]   Install a transparent rust-analyzer shim
  jato rust-analyzer shim uninstall [--dir DIR] Remove the shim and restore any rustup link
  jato rust-analyzer shim status                Show active rust-analyzer shim status
";

fn cmd_shim(args: &[OsString]) -> Result<i32> {
    let sub = args.first().and_then(|a| a.to_str());
    if matches!(sub, Some("-h" | "--help" | "help"))
        || args.iter().any(|a| a == "-h" || a == "--help")
    {
        print!("{SHIM_HELP}");
        return Ok(0);
    }
    let mut explicit_dir: Option<PathBuf> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].to_str() {
            Some("--dir") if i + 1 < args.len() => {
                explicit_dir = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            Some(s) if s.starts_with("--dir=") => {
                explicit_dir = Some(PathBuf::from(&s["--dir=".len()..]));
                i += 1;
            }
            other => bail!("unknown shim argument: {other:?}"),
        }
    }
    match sub {
        Some("install") => shim_install(explicit_dir.as_deref()),
        Some("uninstall") => shim_uninstall(explicit_dir.as_deref()),
        Some("status") | None => shim_status(),
        Some(other) => {
            bail!("unknown shim subcommand `{other}` (expected install|uninstall|status)")
        }
    }
}

fn shim_install(explicit_dir: Option<&Path>) -> Result<i32> {
    let own = env::current_exe().and_then(fs::canonicalize)?;
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let real = resolve_real_ra(&cwd)?;

    let dest = if let Some(dir) = explicit_dir {
        dir.join("rust-analyzer")
    } else if let Some(first) = first_ra_on_path() {
        if is_self(&first) || is_rustup_symlink(&first) {
            first
        } else if let Some(cbin) = cargo_bin_dir()
            && fs::symlink_metadata(cbin.join("rust-analyzer"))
                .ok()
                .is_some_and(|_| is_rustup_symlink(&cbin.join("rust-analyzer")))
        {
            cbin.join("rust-analyzer")
        } else {
            local_bin_dir()
                .context("HOME is not set; pass --dir DIR")?
                .join("rust-analyzer")
        }
    } else if let Some(cbin) = cargo_bin_dir()
        && cbin.is_dir()
    {
        cbin.join("rust-analyzer")
    } else {
        local_bin_dir()
            .context("HOME is not set; pass --dir DIR")?
            .join("rust-analyzer")
    };

    let mut previous_symlink = None;
    if let Ok(meta) = fs::symlink_metadata(&dest) {
        if is_self(&dest) {
            atomic_symlink(&own, &dest)?;
            println!(
                "rust-analyzer shim already installed at {} -> {}",
                dest.display(),
                own.display()
            );
            println!("delegating to {}", real.display());
            return Ok(0);
        }
        if meta.file_type().is_symlink() {
            previous_symlink = fs::read_link(&dest).ok();
        } else {
            bail!(
                "refusing to overwrite non-symlink file at {} (pass --dir DIR to install elsewhere)",
                dest.display()
            );
        }
    }

    atomic_symlink(&own, &dest)?;
    let state = ShimState {
        path: dest.clone(),
        previous_symlink: previous_symlink.clone(),
    };
    if let Ok(state_path) = shim_state_path() {
        let _ = fs::write(state_path, serde_json::to_vec_pretty(&state)?);
    }

    println!(
        "Installed rust-analyzer shim at {} -> {}",
        dest.display(),
        own.display()
    );
    if let Some(prev) = previous_symlink {
        println!(
            "Replaced symlink to {} (uninstall restores it automatically)",
            prev.display()
        );
    }
    println!("Real rust-analyzer: {}", real.display());
    if let Some(active) = first_ra_on_path()
        && !is_self(&active)
    {
        println!(
            "Note: {} appears earlier on PATH; place {} before it to intercept `rust-analyzer`.",
            active.display(),
            dest.parent().unwrap_or(Path::new("")).display()
        );
    }
    Ok(0)
}

fn shim_uninstall(explicit_dir: Option<&Path>) -> Result<i32> {
    let saved: Option<ShimState> = shim_state_path()
        .ok()
        .and_then(|p| fs::read(&p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok());

    let target = explicit_dir
        .map(|d| d.join("rust-analyzer"))
        .or_else(|| saved.as_ref().map(|s| s.path.clone()))
        .or_else(|| first_ra_on_path().filter(|p| is_self(p)))
        .or_else(|| {
            cargo_bin_dir()
                .map(|d| d.join("rust-analyzer"))
                .filter(|p| is_self(p))
        })
        .or_else(|| {
            local_bin_dir()
                .map(|d| d.join("rust-analyzer"))
                .filter(|p| is_self(p))
        });

    let Some(dest) = target else {
        println!("No jato rust-analyzer shim found.");
        return Ok(0);
    };
    if fs::symlink_metadata(&dest).is_err() {
        println!("No rust-analyzer shim present at {}.", dest.display());
        if let Ok(sp) = shim_state_path() {
            let _ = fs::remove_file(sp);
        }
        return Ok(0);
    }
    if !is_self(&dest) {
        bail!(
            "{} does not point to this jato binary; leaving it untouched",
            dest.display()
        );
    }

    let restore = saved
        .as_ref()
        .filter(|s| s.path == dest)
        .and_then(|s| s.previous_symlink.clone())
        .or_else(|| {
            let sibling_rustup = dest.parent()?.join("rustup");
            sibling_rustup.is_file().then(|| PathBuf::from("rustup"))
        });

    if let Some(prev) = restore {
        atomic_symlink(&prev, &dest)?;
        println!(
            "Removed rust-analyzer shim and restored {} -> {}",
            dest.display(),
            prev.display()
        );
    } else {
        fs::remove_file(&dest)?;
        println!("Removed rust-analyzer shim at {}", dest.display());
    }
    if let Ok(sp) = shim_state_path() {
        let _ = fs::remove_file(sp);
    }
    Ok(0)
}

fn shim_status() -> Result<i32> {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let active = first_ra_on_path();
    match &active {
        Some(p) if is_self(p) => {
            println!("Shim       active at {}", p.display());
        }
        Some(p) => {
            println!("Shim       not active (PATH resolves to {})", p.display());
        }
        None => {
            println!("Shim       not installed (no rust-analyzer on PATH)");
        }
    }
    match resolve_real_ra(&cwd) {
        Ok(real) => println!("Backend    {}", real.display()),
        Err(e) => println!("Backend    unavailable ({e})"),
    }
    Ok(0)
}

// ---- socket address, daemon discovery, status & stop -----------------------

fn daemon_socket_name(cache: &Path) -> String {
    let canon = fs::canonicalize(cache).unwrap_or_else(|_| cache.to_path_buf());
    let uid = unsafe { libc::geteuid() };
    let mut h = blake3::Hasher::new_derive_key("jato rust-analyzer daemon v1");
    h.update(&uid.to_le_bytes());
    h.update(canon.as_os_str().as_bytes());
    format!("jato-ra-{}", &h.finalize().to_hex()[..16])
}

fn daemon_addr(cache: &Path) -> io::Result<SocketAddr> {
    SocketAddr::from_abstract_name(daemon_socket_name(cache).as_bytes())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Handshake {
    Status,
    Stop,
    Lsp {
        cwd: PathBuf,
        pid: u32,
        #[serde(default)]
        overrides: ClientEnvOverrides,
        #[serde(default)]
        cli: bool,
    },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ClientEnvOverrides {
    share_worktrees: Option<bool>,
    lazy: Option<bool>,
    threads: Option<usize>,
    prime: Option<bool>,
    proc_macros: Option<usize>,
    lru: Option<usize>,
    target_dir: Option<String>,
    check_on_save: Option<bool>,
    #[serde(default)]
    cache: Option<bool>,
    #[serde(default)]
    seed: Option<bool>,
    #[serde(default)]
    check_via_jato: Option<bool>,
    rustup_toolchain: Option<String>,
    rust_analyzer_bin: Option<PathBuf>,
}

impl ClientEnvOverrides {
    fn from_env() -> Self {
        let parse_bool = |k: &str| {
            env::var(k).ok().and_then(|v| match v.trim() {
                "1" | "true" | "yes" => Some(true),
                "0" | "false" | "no" => Some(false),
                _ => None,
            })
        };
        let parse_usize = |k: &str| env::var(k).ok().and_then(|v| v.trim().parse().ok());
        Self {
            share_worktrees: parse_bool("JATO_RA_SHARE_WORKTREES"),
            lazy: parse_bool("JATO_RA_LAZY"),
            threads: parse_usize("JATO_RA_THREADS"),
            prime: parse_bool("JATO_RA_PRIME"),
            proc_macros: parse_usize("JATO_RA_PROC_MACROS"),
            lru: parse_usize("JATO_RA_LRU"),
            target_dir: env::var("JATO_RA_TARGET_DIR")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            check_on_save: parse_bool("JATO_RA_CHECK_ON_SAVE"),
            cache: parse_bool("JATO_RA_CACHE"),
            seed: parse_bool("JATO_RA_SEED"),
            check_via_jato: parse_bool("JATO_RA_CHECK_JATO"),
            rustup_toolchain: env::var("RUSTUP_TOOLCHAIN")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            rust_analyzer_bin: env::var_os("JATO_RUST_ANALYZER")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct DaemonStatus {
    pid: u32,
    uptime_secs: u64,
    memory_budget_bytes: u64,
    max_backends: usize,
    idle_secs: u64,
    total_rss_bytes: u64,
    clients_connected: usize,
    clients_active: usize,
    clients_dormant: usize,
    backends_reused: u64,
    backends_hibernated: u64,
    backends_evicted: u64,
    #[serde(default)]
    requests_coalesced: u64,
    #[serde(default)]
    cache_hits: u64,
    #[serde(default)]
    backends_seeded: u64,
    #[serde(default)]
    seeded_files_total: u64,
    #[serde(default)]
    cli_queries: u64,
    backends: Vec<BackendStatus>,
}

#[derive(Debug, Serialize, Deserialize)]
struct BackendStatus {
    id: u64,
    pid: u32,
    group: PathBuf,
    ra_bin: PathBuf,
    target_dir: Option<PathBuf>,
    rss_bytes: u64,
    quiescent: bool,
    idle_secs: u64,
    active_clients: usize,
    open_docs: usize,
    #[serde(default)]
    cache_backed: bool,
    #[serde(default)]
    check_via_jato: bool,
    #[serde(default)]
    seeded_files: usize,
    workspaces: Vec<PathBuf>,
}

fn connect_existing(cache: &Path) -> Option<UnixStream> {
    let addr = daemon_addr(cache).ok()?;
    let stream = UnixStream::connect_addr(&addr).ok()?;
    // Authenticate the daemon peer UID (`SO_PEERCRED`) before sending any
    // handshake or workspace source contents over the abstract Unix socket.
    if !wire::same_user(&stream) {
        return None;
    }
    Some(stream)
}

fn connect_or_start_daemon(cache: &Path) -> Result<UnixStream> {
    connect_or_start_daemon_until(cache, Instant::now() + Duration::from_secs(10))
}

fn connect_or_start_daemon_until(cache: &Path, deadline: Instant) -> Result<UnixStream> {
    if let Some(stream) = connect_existing(cache) {
        return Ok(stream);
    }
    let dir = cache.join("rust-analyzer");
    fs::create_dir_all(&dir)?;
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("daemon.lock"))?;
    loop {
        match lock_file.try_lock() {
            Ok(()) => break,
            Err(fs::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    bail!("timed out waiting for jato rust-analyzer daemon startup lock");
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e).context("locking rust-analyzer daemon"),
        }
    }
    if let Some(stream) = connect_existing(cache) {
        return Ok(stream);
    }

    let exe = own_jato_exe().context("locating current jato binary")?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))
        .ok();
    let mut cmd = Command::new(&exe);
    cmd.args(["rust-analyzer", "__daemon"])
        .env("JATO_CACHE_DIR", cache)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log.map_or_else(Stdio::null, Stdio::from));
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("spawning jato rust-analyzer daemon")?;

    while Instant::now() < deadline {
        if let Some(stream) = connect_existing(cache) {
            return Ok(stream);
        }
        thread::sleep(Duration::from_millis(20));
    }
    bail!(
        "timed out waiting for jato rust-analyzer daemon (see {})",
        dir.join("daemon.log").display()
    )
}

fn cmd_status(json_out: bool) -> Result<i32> {
    let cache = cache_dir()?;
    let Some(mut stream) = connect_existing(&cache) else {
        if json_out {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({"running": false}))?
            );
        } else {
            println!("Daemon     not running");
            let _ = shim_status();
        }
        return Ok(0);
    };
    let req = serde_json::to_vec(&Handshake::Status)?;
    wire::send(&mut stream, &req)?;
    let reply = wire::receive(&mut stream)?;
    let status: DaemonStatus = serde_json::from_slice(&reply)?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(0);
    }
    let mib = |b: u64| format!("{:.1} MiB", b as f64 / 1048576.0);
    println!("Daemon     pid {}, up {} s", status.pid, status.uptime_secs);
    println!(
        "Clients    {} connected ({} active, {} dormant/hibernated)",
        status.clients_connected, status.clients_active, status.clients_dormant
    );
    println!(
        "Memory     {} across {} backend(s) (budget {}, max {})",
        mib(status.total_rss_bytes),
        status.backends.len(),
        if status.memory_budget_bytes == 0 {
            "unlimited".to_owned()
        } else {
            mib(status.memory_budget_bytes)
        },
        if status.max_backends == 0 {
            "unlimited".to_owned()
        } else {
            status.max_backends.to_string()
        }
    );
    println!(
        "Sharing    {} shared attaches, {} coalesced, {} cache hits, {} CLI queries, {} seeded ({} files), {} hibernated, {} evicted (idle {} s)",
        status.backends_reused,
        status.requests_coalesced,
        status.cache_hits,
        status.cli_queries,
        status.backends_seeded,
        status.seeded_files_total,
        status.backends_hibernated,
        status.backends_evicted,
        status.idle_secs
    );
    for b in &status.backends {
        let mut tags = Vec::new();
        if b.cache_backed {
            tags.push("cache");
        }
        if b.check_via_jato {
            tags.push("jato-check");
        }
        let state = if b.pid == 0 {
            "seeding"
        } else if b.quiescent {
            "ready"
        } else {
            "loading"
        };
        let extra = if tags.is_empty() && b.seeded_files == 0 {
            String::new()
        } else {
            let mut parts: Vec<String> = tags.into_iter().map(str::to_owned).collect();
            if b.seeded_files > 0 {
                parts.push(format!("seeded={}", b.seeded_files));
            }
            format!(" [{}]", parts.join(","))
        };
        println!(
            "  #{} pid={} rss={} clients={} docs={} {}{} group={}",
            b.id,
            b.pid,
            mib(b.rss_bytes),
            b.active_clients,
            b.open_docs,
            state,
            extra,
            b.group.display()
        );
        for ws in &b.workspaces {
            println!("      workspace {}", ws.display());
        }
    }
    let _ = shim_status();
    Ok(0)
}

fn cmd_stop() -> Result<i32> {
    let cache = cache_dir()?;
    let Some(mut stream) = connect_existing(&cache) else {
        println!("jato rust-analyzer daemon is not running.");
        return Ok(0);
    };
    let req = serde_json::to_vec(&Handshake::Stop)?;
    wire::send(&mut stream, &req)?;
    let _ = wire::receive(&mut stream);
    println!("Stopped jato rust-analyzer daemon.");
    Ok(0)
}

// ---- thin stdio <-> Unix socket LSP proxy ----------------------------------

fn run_proxy(_args: &[OsString]) -> Result<i32> {
    let cache = cache_dir()?;
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let handshake = Handshake::Lsp {
        cwd,
        pid: std::process::id(),
        overrides: ClientEnvOverrides::from_env(),
        cli: false,
    };
    let hs_bytes = serde_json::to_vec(&handshake)?;

    // Retry briefly if the daemon was shutting down right as we connected.
    let mut stream = None;
    let mut last_err = None;
    for _ in 0..10 {
        match connect_or_start_daemon(&cache) {
            Ok(mut s) => {
                if wire::send(&mut s, &hs_bytes).is_ok() && wire::receive(&mut s).is_ok() {
                    stream = Some(s);
                    break;
                }
                last_err = Some(anyhow::anyhow!("daemon closed before handshake ack"));
            }
            Err(e) => last_err = Some(e),
        }
        thread::sleep(Duration::from_millis(50));
    }
    let stream = match stream {
        Some(s) => s,
        None => return Err(last_err.unwrap_or_else(|| anyhow::anyhow!("connect failed"))),
    };

    let mut reader = stream.try_clone()?;
    let mut writer = stream;

    let stdin_to_sock = thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = [0u8; 32 * 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if writer.write_all(&buf[..n]).is_err() || writer.flush().is_err() {
                        break;
                    }
                }
            }
        }
        let _ = writer.shutdown(std::net::Shutdown::Write);
    });

    let mut stdout = io::stdout().lock();
    let mut buf = [0u8; 32 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if stdout.write_all(&buf[..n]).is_err() || stdout.flush().is_err() {
                    break;
                }
            }
        }
    }
    // Do not block forever on `stdin_to_sock.join()`: if the daemon stopped
    // while the client's stdin pipe is still open, exiting promptly lets the
    // editor/agent detect EOF and respawn cleanly.
    drop(stdin_to_sock);
    Ok(0)
}

// ---- LSP framing, URI helpers, git common dir, and document sync -----------

fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    limit: usize,
) -> io::Result<usize> {
    buf.clear();
    loop {
        let avail = reader.fill_buf()?;
        if avail.is_empty() {
            return Ok(buf.len());
        }
        if let Some(pos) = memchr::memchr(b'\n', avail) {
            let take = pos + 1;
            if buf.len() + take > limit {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "LSP header line exceeds limit",
                ));
            }
            buf.extend_from_slice(&avail[..take]);
            reader.consume(take);
            return Ok(buf.len());
        }
        let take = avail.len();
        if buf.len() + take > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "LSP header line exceeds limit",
            ));
        }
        buf.extend_from_slice(avail);
        reader.consume(take);
    }
}

fn read_lsp_message<R: BufRead>(reader: &mut R) -> io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    let mut raw_line = Vec::with_capacity(128);
    let mut saw_header = false;
    let mut header_count = 0usize;
    loop {
        let n = read_bounded_line(reader, &mut raw_line, MAX_LSP_HEADER_LINE)?;
        if n == 0 {
            return if saw_header {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated LSP header",
                ))
            } else {
                Ok(None)
            };
        }
        saw_header = true;
        let line = std::str::from_utf8(&raw_line)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        header_count += 1;
        if header_count > MAX_LSP_HEADERS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "too many LSP headers",
            ));
        }
        if let Some((k, v)) = trimmed.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            let v = v.trim();
            content_length = if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) {
                v.parse::<usize>().ok()
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid Content-Length header",
                ));
            };
        }
    }
    let len = content_length
        .filter(|&l| l <= MAX_LSP_MESSAGE)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "missing/invalid Content-Length")
        })?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    let msg: Value =
        serde_json::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if !msg.is_object() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "LSP JSON-RPC payload must be an object",
        ));
    }
    Ok(Some(msg))
}

fn encode_lsp_message(msg: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(msg).unwrap_or_default();
    let mut out = Vec::with_capacity(body.len() + 32);
    let _ = write!(out, "Content-Length: {}\r\n\r\n", body.len());
    out.extend_from_slice(&body);
    out
}

fn path_to_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        let safe = b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~');
        if safe {
            out.push(b as char);
        } else {
            let _ = std::fmt::Write::write_fmt(&mut out, format_args!("%{b:02X}"));
        }
    }
    out
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest
        .strip_prefix("localhost")
        .filter(|r| r.starts_with('/'))
        .unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
        {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            if let Ok(val) = u8::from_str_radix(hex, 16) {
                out.push(val);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(OsStr::from_bytes(&out)))
}

fn uri_in_root(uri: &str, root_uri: &str) -> bool {
    let root = if root_uri.len() > "file:///".len() {
        root_uri.trim_end_matches('/')
    } else {
        root_uri
    };
    if !uri.contains('%')
        && !root.contains('%')
        && !uri.starts_with("file://localhost/")
        && !root.starts_with("file://localhost/")
    {
        if root == "file:///" {
            return uri.starts_with("file:///");
        }
        return uri == root
            || (uri.starts_with(root)
                && uri.as_bytes().get(root.len()).is_some_and(|&b| b == b'/'));
    }
    match (uri_to_path(uri), uri_to_path(root)) {
        (Some(p), Some(r)) => p.starts_with(r),
        _ => {
            uri == root
                || (uri.starts_with(root)
                    && uri.as_bytes().get(root.len()).is_some_and(|&b| b == b'/'))
        }
    }
}

/// Find the git common directory shared by a repo and all its git worktrees.
/// Returns `None` if `dir` is not inside a git repository.
fn git_common_dir(dir: &Path) -> Option<PathBuf> {
    for anc in dir.ancestors() {
        let dot_git = anc.join(".git");
        let Ok(meta) = fs::symlink_metadata(&dot_git) else {
            continue;
        };
        if meta.is_dir() {
            return Some(fs::canonicalize(&dot_git).unwrap_or(dot_git));
        }
        if meta.is_file()
            && let Ok(text) = fs::read_to_string(&dot_git)
            && let Some(raw) = text
                .lines()
                .find_map(|l| l.trim().strip_prefix("gitdir:"))
                .map(str::trim)
        {
            let gitdir = if Path::new(raw).is_absolute() {
                PathBuf::from(raw)
            } else {
                anc.join(raw)
            };
            let common = fs::read_to_string(gitdir.join("commondir"))
                .ok()
                .map(|c| {
                    let c = c.trim();
                    if Path::new(c).is_absolute() {
                        PathBuf::from(c)
                    } else {
                        gitdir.join(c)
                    }
                })
                .unwrap_or(gitdir);
            return Some(fs::canonicalize(&common).unwrap_or(common));
        }
    }
    None
}

/// Convert an LSP UTF-16 `(line, character)` position to a UTF-8 byte offset.
fn lsp_pos_to_byte(text: &str, target_line: u64, target_utf16: u64) -> usize {
    let mut offset = 0usize;
    for (line, chunk) in text.split_inclusive('\n').enumerate() {
        if line as u64 == target_line {
            let without_lf = chunk.strip_suffix('\n').unwrap_or(chunk);
            let line_str = without_lf.strip_suffix('\r').unwrap_or(without_lf);
            let mut utf16 = 0u64;
            for (byte_idx, ch) in line_str.char_indices() {
                if utf16 >= target_utf16 {
                    return offset + byte_idx;
                }
                utf16 += ch.len_utf16() as u64;
            }
            return offset + line_str.len();
        }
        offset += chunk.len();
    }
    text.len()
}

/// Apply LSP `contentChanges` to an in-memory document buffer.
fn apply_content_changes(text: &mut String, changes: &[Value]) {
    for ch in changes {
        let Some(new_text) = ch.get("text").and_then(Value::as_str) else {
            continue;
        };
        let Some(range) = ch.get("range") else {
            text.clear();
            text.push_str(new_text);
            continue;
        };
        let (Some(sl), Some(sc), Some(el), Some(ec)) = (
            range.pointer("/start/line").and_then(Value::as_u64),
            range.pointer("/start/character").and_then(Value::as_u64),
            range.pointer("/end/line").and_then(Value::as_u64),
            range.pointer("/end/character").and_then(Value::as_u64),
        ) else {
            continue;
        };
        let start = lsp_pos_to_byte(text, sl, sc);
        let end = lsp_pos_to_byte(text, el, ec).max(start);
        text.replace_range(start..end, new_text);
    }
}

fn deep_merge(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (slot, val) => *slot = val.clone(),
    }
}

fn config_section(cfg: &Value, section: Option<&str>) -> Value {
    let Some(section) = section else {
        return cfg.clone();
    };
    let trimmed = section
        .strip_prefix("rust-analyzer.")
        .or_else(|| (section == "rust-analyzer").then_some(""));
    let Some(path) = trimmed else {
        return Value::Null;
    };
    if path.is_empty() {
        return cfg.clone();
    }
    let mut cur = cfg;
    for part in path.split('.') {
        match cur.get(part) {
            Some(next) => cur = next,
            None => return Value::Null,
        }
    }
    cur.clone()
}

/// Filter LSP response values so worktree-shared backends never leak locations
/// or symbols belonging to a sibling worktree.
fn filter_response_for_workspace(method: &str, result: &mut Value, other_root_uris: &[String]) {
    if other_root_uris.is_empty() {
        return;
    }
    let is_sibling = |uri: &str| other_root_uris.iter().any(|r| uri_in_root(uri, r));
    match method {
        "workspace/symbol" => {
            if let Some(arr) = result.as_array_mut() {
                arr.retain(|item| {
                    item.pointer("/location/uri")
                        .and_then(Value::as_str)
                        .is_none_or(|u| !is_sibling(u))
                });
            }
        }
        "textDocument/references"
        | "textDocument/implementation"
        | "textDocument/definition"
        | "textDocument/declaration"
        | "textDocument/typeDefinition" => {
            if let Some(arr) = result.as_array_mut() {
                arr.retain(|item| {
                    let uri = item
                        .get("uri")
                        .or_else(|| item.get("targetUri"))
                        .and_then(Value::as_str);
                    uri.is_none_or(|u| !is_sibling(u))
                });
            }
        }
        "callHierarchy/incomingCalls" => {
            if let Some(arr) = result.as_array_mut() {
                arr.retain(|item| {
                    item.pointer("/from/uri")
                        .and_then(Value::as_str)
                        .is_none_or(|u| !is_sibling(u))
                });
            }
        }
        "callHierarchy/outgoingCalls" => {
            if let Some(arr) = result.as_array_mut() {
                arr.retain(|item| {
                    item.pointer("/to/uri")
                        .and_then(Value::as_str)
                        .is_none_or(|u| !is_sibling(u))
                });
            }
        }
        "typeHierarchy/supertypes" | "typeHierarchy/subtypes" => {
            if let Some(arr) = result.as_array_mut() {
                arr.retain(|item| {
                    item.get("uri")
                        .and_then(Value::as_str)
                        .is_none_or(|u| !is_sibling(u))
                });
            }
        }
        "textDocument/rename" => {
            if let Some(changes) = result.get_mut("changes").and_then(Value::as_object_mut) {
                changes.retain(|uri, _| !is_sibling(uri));
            }
            if let Some(doc_changes) = result
                .get_mut("documentChanges")
                .and_then(Value::as_array_mut)
            {
                doc_changes.retain(|edit| {
                    let uri = edit
                        .pointer("/textDocument/uri")
                        .or_else(|| edit.get("uri"))
                        .or_else(|| edit.get("oldUri"))
                        .and_then(Value::as_str);
                    uri.is_none_or(|u| !is_sibling(u))
                });
            }
        }
        _ => (),
    }
}

// ---- /proc RSS & system memory helpers -------------------------------------

fn system_memory_bytes() -> u64 {
    let Ok(text) = fs::read_to_string("/proc/meminfo") else {
        return 16 << 30;
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:")
            && let Some(kb) = rest
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<u64>().ok())
        {
            return kb * 1024;
        }
    }
    16 << 30
}

fn proc_rss_bytes(pid: u32) -> u64 {
    let Ok(text) = fs::read_to_string(format!("/proc/{pid}/status")) else {
        return 0;
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:")
            && let Some(kb) = rest
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<u64>().ok())
        {
            return kb * 1024;
        }
    }
    0
}

/// Measure total RSS (main process + proc-macro-srv / helper children in the
/// same process group) for each tracked backend pid in one `/proc` pass.
fn measure_group_rss(pids: &[u32]) -> HashMap<u32, u64> {
    let targets: HashSet<u32> = pids.iter().copied().collect();
    let mut totals: HashMap<u32, u64> = pids.iter().map(|&p| (p, 0)).collect();
    let Ok(entries) = fs::read_dir("/proc") else {
        return totals;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        // After ')': state(0) ppid(1) pgrp(2)
        let mut fields = rest.split_whitespace();
        let _state = fields.next();
        let ppid = fields.next().and_then(|s| s.parse::<u32>().ok());
        let pgrp = fields.next().and_then(|s| s.parse::<u32>().ok());
        let owner = pgrp
            .filter(|g| targets.contains(g))
            .or_else(|| targets.contains(&pid).then_some(pid))
            .or_else(|| ppid.filter(|p| targets.contains(p)));
        if let Some(root) = owner {
            *totals.entry(root).or_insert(0) += proc_rss_bytes(pid);
        }
    }
    totals
}

// ---- capability probing & caching ------------------------------------------

fn caps_cache_path(cache: &Path, ra_bin: &Path) -> PathBuf {
    let meta = fs::metadata(ra_bin).ok();
    let mtime = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    let len = meta.as_ref().map_or(0, fs::Metadata::len);
    let mut h = blake3::Hasher::new_derive_key("jato ra caps v3");
    h.update(ra_bin.as_os_str().as_bytes());
    h.update(&mtime.to_le_bytes());
    h.update(&len.to_le_bytes());
    cache
        .join("rust-analyzer")
        .join(format!("caps-{}.json", &h.finalize().to_hex()[..16]))
}

fn probe_ra_capabilities(cache: &Path, ra_bin: &Path, probe_id: u64) -> Result<Value> {
    let cache_path = caps_cache_path(cache, ra_bin);
    if let Ok(bytes) = fs::read(&cache_path)
        && let Ok(val) = serde_json::from_slice::<Value>(&bytes)
        && val.get("capabilities").is_some()
    {
        return Ok(val);
    }

    let empty = cache
        .join("rust-analyzer")
        .join(format!("probe-{}-{probe_id}", std::process::id()));
    let _ = fs::remove_dir_all(&empty);
    fs::create_dir_all(&empty)?;

    let mut child = Command::new(ra_bin)
        .current_dir(&empty)
        .env("JATO_RA_PASSTHROUGH", "1")
        .env("MALLOC_ARENA_MAX", "2")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .with_context(|| format!("probing capabilities from {}", ra_bin.display()))?;
    let pgrp = child.id() as libc::pid_t;

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "processId": std::process::id(),
            "rootUri": path_to_uri(&empty),
            "workspaceFolders": [],
            "capabilities": {
                "workspace": {
                    "workspaceFolders": true,
                    "configuration": true
                }
            }
        }
    });
    let _ = stdin.write_all(&encode_lsp_message(&init));
    let _ = stdin.flush();

    let mut result = None;
    while let Ok(Some(msg)) = read_lsp_message(&mut stdout) {
        if msg.get("id").and_then(Value::as_u64) == Some(1) {
            result = msg.get("result").cloned();
            break;
        }
    }
    let _ = stdin.write_all(&encode_lsp_message(&json!({
        "jsonrpc": "2.0", "method": "initialized", "params": {}
    })));
    let _ = stdin.write_all(&encode_lsp_message(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "shutdown", "params": null
    })));
    let _ = stdin.flush();
    drop(stdin);
    unsafe {
        libc::kill(-pgrp, libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_dir_all(&empty);

    let result = result.context("rust-analyzer did not return InitializeResult during probe")?;
    write_caps_cache(&cache_path, &result, probe_id);
    Ok(result)
}

fn write_caps_cache(cache_path: &Path, result: &Value, salt: u64) {
    let tmp = cache_path.with_extension(format!("{}-{salt}.tmp", std::process::id()));
    if let Ok(bytes) = serde_json::to_vec(result)
        && fs::write(&tmp, bytes).is_ok()
        && fs::rename(&tmp, cache_path).is_err()
    {
        let _ = fs::remove_file(&tmp);
    }
}

// ---- daemon configuration --------------------------------------------------

#[derive(Clone, Debug)]
struct DaemonConfig {
    cache_dir: PathBuf,
    target_root: PathBuf,
    memory_budget: u64,
    max_backends: usize,
    idle_timeout: Duration,
    share_worktrees: bool,
    lazy: bool,
    threads: usize,
    prime: bool,
    proc_macros: usize,
    lru_capacity: usize,
    shared_target_dir: Option<String>,
    check_on_save: Option<bool>,
    cache: bool,
    seed: bool,
    check_via_jato: bool,
}

#[derive(Clone, Debug)]
struct EffectiveRaConfig {
    target_dir: Option<PathBuf>,
    check_target_dir: Option<PathBuf>,
    cache_backed: bool,
    seed: bool,
    check_via_jato: bool,
    cargo_extra_env: BTreeMap<OsString, OsString>,
}

fn client_customized_cargo(client_opts: &Value) -> bool {
    let cargo_keys = [
        "features",
        "allFeatures",
        "noDefaultFeatures",
        "target",
        "allTargets",
        "extraArgs",
        "cfgs",
    ];
    client_opts
        .get("cargo")
        .and_then(Value::as_object)
        .is_some_and(|m| m.keys().any(|k| cargo_keys.contains(&k.as_str())))
}

fn client_customized_check(client_opts: &Value) -> bool {
    let custom_keys = [
        "overrideCommand",
        "command",
        "extraArgs",
        "features",
        "allFeatures",
        "allTargets",
        "targets",
        "workspace",
        "noDefaultFeatures",
        "invocationStrategy",
        "invocationLocation",
    ];
    let has_custom_check = client_opts
        .get("check")
        .and_then(Value::as_object)
        .is_some_and(|m| m.keys().any(|k| custom_keys.contains(&k.as_str())));
    let has_legacy_check = client_opts
        .get("checkOnSave")
        .and_then(Value::as_object)
        .is_some_and(|m| m.keys().any(|k| custom_keys.contains(&k.as_str())));
    has_custom_check || has_legacy_check || client_customized_cargo(client_opts)
}

fn ensure_object<'a>(val: &'a mut Value, key: &str) -> &'a mut Map<String, Value> {
    if !val.is_object() {
        *val = Value::Object(Map::new());
    }
    let map = val.as_object_mut().unwrap();
    let entry = map
        .entry(key.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(Map::new());
    }
    entry.as_object_mut().unwrap()
}

fn ensure_nested_object<'a>(
    map: &'a mut Map<String, Value>,
    key: &str,
) -> &'a mut Map<String, Value> {
    let entry = map
        .entry(key.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(Map::new());
    }
    entry.as_object_mut().unwrap()
}

fn sibling_check_target_dir(target_dir: &Path) -> PathBuf {
    let mut name = target_dir
        .file_name()
        .map(OsStr::to_os_string)
        .unwrap_or_else(|| OsString::from("ra-target"));
    name.push("-check");
    target_dir.with_file_name(name)
}

fn own_jato_exe() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    let raw = exe.to_string_lossy();
    if let Some(stripped) = raw.strip_suffix(" (deleted)") {
        let p = PathBuf::from(stripped);
        if is_executable_file(&p) {
            return Some(fs::canonicalize(&p).unwrap_or(p));
        }
        return None;
    }
    let canon = fs::canonicalize(&exe).unwrap_or(exe);
    is_executable_file(&canon).then_some(canon)
}

impl DaemonConfig {
    fn from_env(cache_dir: PathBuf) -> Self {
        let cpus = thread::available_parallelism().map_or(8, |n| n.get());
        let default_threads = (cpus / 8).clamp(4, 8);
        let default_mem = (system_memory_bytes() / 4).clamp(4 << 30, 32 << 30);

        let memory_budget = match env::var("JATO_RA_MEMORY") {
            Ok(v) if v.trim() == "0" => 0,
            Ok(v) if !v.trim().is_empty() => parse_size(v.trim()).unwrap_or(default_mem),
            _ => default_mem,
        };
        let max_backends = env::var("JATO_RA_MAX_BACKENDS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(8);
        let idle_secs = env::var("JATO_RA_IDLE_SECS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(600);
        let parse_bool = |k: &str, default: bool| {
            env::var(k)
                .ok()
                .and_then(|v| match v.trim() {
                    "1" | "true" | "yes" => Some(true),
                    "0" | "false" | "no" => Some(false),
                    _ => None,
                })
                .unwrap_or(default)
        };
        let target_name = format!(
            "{}-ra-targets",
            cache_dir
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or("jato")
        );
        let target_root = cache_dir.with_file_name(target_name);
        Self {
            cache_dir,
            target_root,
            memory_budget,
            max_backends,
            idle_timeout: Duration::from_secs(idle_secs),
            share_worktrees: parse_bool("JATO_RA_SHARE_WORKTREES", true),
            lazy: parse_bool("JATO_RA_LAZY", true),
            threads: env::var("JATO_RA_THREADS")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .filter(|&n| n > 0)
                .unwrap_or(default_threads),
            prime: parse_bool("JATO_RA_PRIME", false),
            proc_macros: env::var("JATO_RA_PROC_MACROS")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .filter(|&n| n > 0)
                .unwrap_or(1),
            lru_capacity: env::var("JATO_RA_LRU")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .filter(|&n| n > 0)
                .unwrap_or(128),
            shared_target_dir: env::var("JATO_RA_TARGET_DIR")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            check_on_save: env::var("JATO_RA_CHECK_ON_SAVE")
                .ok()
                .and_then(|v| match v.trim() {
                    "1" | "true" | "yes" => Some(true),
                    "0" | "false" | "no" => Some(false),
                    _ => None,
                }),
            cache: parse_bool("JATO_RA_CACHE", true),
            seed: parse_bool("JATO_RA_SEED", true),
            check_via_jato: parse_bool("JATO_RA_CHECK_JATO", true),
        }
    }

    fn backend_target_dir(
        &self,
        group: &Path,
        rustc_bin: &Path,
        workspace_root: &Path,
        ov: &ClientEnvOverrides,
    ) -> Option<PathBuf> {
        let setting = ov
            .target_dir
            .as_deref()
            .or(self.shared_target_dir.as_deref());
        match setting.map(str::trim) {
            Some("0" | "false" | "off") => None,
            Some("1" | "true" | "on") | None => {
                let mut h = blake3::Hasher::new_derive_key("jato ra target v1");
                h.update(group.as_os_str().as_bytes());
                h.update(&[0]);
                h.update(rustc_bin.as_os_str().as_bytes());
                Some(self.target_root.join(&h.finalize().to_hex()[..16]))
            }
            Some(custom) => {
                let p = PathBuf::from(custom);
                Some(if p.is_absolute() {
                    p
                } else {
                    workspace_root.join(p)
                })
            }
        }
    }

    fn merged_ra_settings(
        &self,
        group: &Path,
        rustc_bin: &Path,
        workspace_root: &Path,
        client_opts: &Value,
        ov: &ClientEnvOverrides,
    ) -> (Value, EffectiveRaConfig) {
        self.merged_ra_settings_with_exe(
            group,
            rustc_bin,
            workspace_root,
            client_opts,
            ov,
            own_jato_exe().as_deref(),
        )
    }

    fn merged_ra_settings_with_exe(
        &self,
        group: &Path,
        rustc_bin: &Path,
        workspace_root: &Path,
        client_opts: &Value,
        ov: &ClientEnvOverrides,
        jato_exe: Option<&Path>,
    ) -> (Value, EffectiveRaConfig) {
        let threads = ov.threads.unwrap_or(self.threads);
        let prime = ov.prime.unwrap_or(self.prime);
        let proc_macros = ov.proc_macros.unwrap_or(self.proc_macros);
        let lru = ov.lru.unwrap_or(self.lru_capacity);
        let check_on_save = ov.check_on_save.or(self.check_on_save);
        let target_dir = self.backend_target_dir(group, rustc_bin, workspace_root, ov);
        let cache_wanted = ov.cache.unwrap_or(self.cache) && jato_exe.is_some();

        let mut base = json!({
            "numThreads": threads,
            "cachePriming": {
                "enable": prime,
                "numThreads": threads
            },
            "procMacro": {
                "processes": proc_macros
            },
            "lru": {
                "capacity": lru
            }
        });
        if let Some(ref dir) = target_dir {
            base["cargo"] = json!({ "targetDir": dir.to_string_lossy() });
        }
        if cache_wanted && let Some(exe) = jato_exe {
            let cargo = ensure_object(&mut base, "cargo");
            let extra = ensure_nested_object(cargo, "extraEnv");
            extra.insert(
                "RUSTC_WRAPPER".to_owned(),
                Value::String(exe.to_string_lossy().into_owned()),
            );
            extra.insert(
                "JATO_CACHE_DIR".to_owned(),
                Value::String(self.cache_dir.to_string_lossy().into_owned()),
            );
        }
        if let Some(cos) = check_on_save {
            base["checkOnSave"] = Value::Bool(cos);
        }
        // Client explicit initializationOptions take precedence over Jato's
        // default tuning, while explicit per-run JATO_RA_* env vars win over both.
        if client_opts.is_object() {
            deep_merge(&mut base, client_opts);
        }
        if let Some(t) = ov.threads {
            base["numThreads"] = json!(t);
            base["cachePriming"]["numThreads"] = json!(t);
        }
        if let Some(p) = ov.prime {
            base["cachePriming"]["enable"] = Value::Bool(p);
        }
        if let Some(pm) = ov.proc_macros {
            base["procMacro"]["processes"] = json!(pm);
        }
        if let Some(l) = ov.lru {
            base["lru"]["capacity"] = json!(l);
        }
        if let Some(cos) = ov.check_on_save {
            base["checkOnSave"] = Value::Bool(cos);
        }
        if ov.cache == Some(true)
            && let Some(exe) = jato_exe
        {
            let cargo = ensure_object(&mut base, "cargo");
            let extra = ensure_nested_object(cargo, "extraEnv");
            extra.insert(
                "RUSTC_WRAPPER".to_owned(),
                Value::String(exe.to_string_lossy().into_owned()),
            );
            extra.insert(
                "JATO_CACHE_DIR".to_owned(),
                Value::String(self.cache_dir.to_string_lossy().into_owned()),
            );
        }

        // Determine effective cargo.targetDir after client overrides.
        // Note: `cargo.targetDir: true` tells rust-analyzer to use a subdir
        // inside Cargo's target dir, which we leave untouched rather than
        // aliasing onto Jato's shared target dir.
        let effective_target_dir = match base.pointer("/cargo/targetDir") {
            Some(Value::String(s)) if !s.trim().is_empty() => {
                let p = PathBuf::from(s);
                Some(if p.is_absolute() {
                    p
                } else {
                    workspace_root.join(p)
                })
            }
            Some(Value::Bool(_)) => None,
            _ => target_dir,
        };

        // Compute effective_cache_backed AFTER client overrides so overriding
        // `cargo.extraEnv.RUSTC_WRAPPER` disables seeding and jato-check too.
        let effective_cache_backed = cache_wanted
            && jato_exe.is_some_and(|exe| {
                base.pointer("/cargo/extraEnv/RUSTC_WRAPPER")
                    .and_then(Value::as_str)
                    .is_some_and(|w| Path::new(w) == exe)
            });

        // Collect effective `cargo.extraEnv` string values for seed compatibility.
        let mut cargo_extra_env = BTreeMap::new();
        let mut custom_cache_dir = false;
        if let Some(extra) = base.pointer("/cargo/extraEnv").and_then(Value::as_object) {
            for (k, v) in extra {
                if let Some(val) = v.as_str() {
                    if k == "JATO_CACHE_DIR" && Path::new(val) != self.cache_dir {
                        custom_cache_dir = true;
                    }
                    cargo_extra_env.insert(OsString::from(k), OsString::from(val));
                }
            }
        }

        // Route checkOnSave through `jato check --workspace --all-targets --message-format=json`
        // into an isolated sibling `-check` target dir (so Jato restores never race
        // with rust-analyzer's own direct `cargo` build-script/proc-macro runs).
        let check_on_save_disabled = base.get("checkOnSave") == Some(&Value::Bool(false))
            || base.pointer("/checkOnSave/enable") == Some(&Value::Bool(false));
        let client_has_override = client_opts.pointer("/check/overrideCommand").is_some()
            || client_opts
                .pointer("/checkOnSave/overrideCommand")
                .is_some();
        let check_target_dir = effective_target_dir
            .as_deref()
            .map(sibling_check_target_dir);
        let want_check_jato =
            effective_cache_backed && ov.check_via_jato.unwrap_or(self.check_via_jato);
        // Without a dedicated target dir, only inject check.overrideCommand if
        // the user explicitly forced JATO_RA_CHECK_JATO=1 (avoids contending
        // with the workspace's primary `target/` by default).
        let target_ok_for_check = check_target_dir.is_some() || ov.check_via_jato == Some(true);
        let allow_override = want_check_jato
            && target_ok_for_check
            && !check_on_save_disabled
            && !client_has_override
            && (ov.check_via_jato == Some(true) || !client_customized_check(client_opts));

        let mut check_via_jato_active = false;
        if allow_override && let Some(exe) = jato_exe {
            let has_cargo_target_env = base.pointer("/cargo/extraEnv/CARGO_TARGET_DIR").is_some();
            let check = ensure_object(&mut base, "check");
            check.insert(
                "overrideCommand".to_owned(),
                json!([
                    exe.to_string_lossy(),
                    "check",
                    "--workspace",
                    "--all-targets",
                    "--message-format=json"
                ]),
            );
            if let Some(ref dir) = check_target_dir {
                let has_check_target_env = check
                    .get("extraEnv")
                    .and_then(|v| v.get("CARGO_TARGET_DIR"))
                    .is_some();
                if !has_cargo_target_env && !has_check_target_env {
                    let extra = ensure_nested_object(check, "extraEnv");
                    extra.insert(
                        "CARGO_TARGET_DIR".to_owned(),
                        Value::String(dir.to_string_lossy().into_owned()),
                    );
                }
            }
            check_via_jato_active = true;
        }

        let seed_enabled = effective_cache_backed
            && ov.seed.unwrap_or(self.seed)
            && effective_target_dir.is_some()
            && !custom_cache_dir
            && !client_customized_cargo(client_opts);

        let effective = EffectiveRaConfig {
            target_dir: effective_target_dir,
            check_target_dir: check_via_jato_active.then_some(check_target_dir).flatten(),
            cache_backed: effective_cache_backed,
            seed: seed_enabled,
            check_via_jato: check_via_jato_active,
            cargo_extra_env,
        };
        (base, effective)
    }
}

// ---- daemon actor state ----------------------------------------------------

type ClientId = u64;
type BackendId = u64;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct BackendKey {
    group: PathBuf,
    ra_bin: PathBuf,
    rustc_bin: PathBuf,
    settings_hash: [u8; 32],
}

fn settings_fingerprint(ra_settings: &Value, effective: &EffectiveRaConfig) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key("jato ra settings v1");
    if let Ok(bytes) = serde_json::to_vec(ra_settings) {
        h.update(&bytes);
    }
    h.update(&[
        u8::from(effective.cache_backed),
        u8::from(effective.check_via_jato),
    ]);
    if let Some(ref dir) = effective.target_dir {
        h.update(dir.as_os_str().as_bytes());
    }
    *h.finalize().as_bytes()
}

/// Owns a spawned `rust-analyzer` child process group and guarantees it is
/// killed and reaped on drop even if a queued `BackendSpawned` event is
/// discarded during daemon shutdown.
struct ManagedChild {
    child: Option<Child>,
    pid: u32,
}

impl ManagedChild {
    fn new(child: Child) -> Self {
        let pid = child.id();
        Self {
            child: Some(child),
            pid,
        }
    }

    fn take(&mut self) -> Option<Child> {
        self.child.take()
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let pgrp = self.pid as libc::pid_t;
            unsafe {
                libc::kill(-pgrp, libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[derive(Clone, Debug)]
struct ClientDoc {
    language_id: String,
    text: String,
}

struct ClientOutQueue {
    responses: VecDeque<Vec<u8>>,
    notifications: VecDeque<Vec<u8>>,
    closed: bool,
}

/// Non-blocking, two-lane outbound writer for one LSP client.
///
/// RPC responses (`id` present) take priority and are never crowded out by
/// unsolicited server notifications (`publishDiagnostics`, `$/progress`),
/// while notifications are capped to keep memory bounded even if a client is
/// `SIGSTOP`ped or slow to read stdout.
struct ClientWriter {
    inner: Arc<(Mutex<ClientOutQueue>, Condvar)>,
}

impl ClientWriter {
    fn spawn(mut stream: UnixStream) -> Self {
        let inner = Arc::new((
            Mutex::new(ClientOutQueue {
                responses: VecDeque::new(),
                notifications: VecDeque::new(),
                closed: false,
            }),
            Condvar::new(),
        ));
        let worker = Arc::clone(&inner);
        thread::spawn(move || {
            let (lock, cv) = &*worker;
            loop {
                let frame = {
                    let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
                    loop {
                        if let Some(f) = q.responses.pop_front() {
                            break Some(f);
                        }
                        if q.closed {
                            break None;
                        }
                        if let Some(f) = q.notifications.pop_front() {
                            break Some(f);
                        }
                        q = cv.wait(q).unwrap_or_else(|e| e.into_inner());
                    }
                };
                let Some(bytes) = frame else {
                    break;
                };
                if stream.write_all(&bytes).is_err() || stream.flush().is_err() {
                    let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
                    q.closed = true;
                    q.responses.clear();
                    q.notifications.clear();
                    break;
                }
            }
        });
        Self { inner }
    }

    fn send(&self, msg: &Value) {
        let is_response = msg.get("id").is_some();
        let frame = encode_lsp_message(msg);
        let (lock, cv) = &*self.inner;
        let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
        if q.closed {
            return;
        }
        if is_response {
            if q.responses.len() >= MAX_CLIENT_RESPONSES {
                q.closed = true;
                q.responses.clear();
                q.notifications.clear();
                cv.notify_one();
                return;
            }
            q.responses.push_back(frame);
        } else {
            while q.notifications.len() >= MAX_CLIENT_NOTIFS {
                q.notifications.pop_front();
            }
            q.notifications.push_back(frame);
        }
        cv.notify_one();
    }

    fn close(&self) {
        let (lock, cv) = &*self.inner;
        let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
        q.closed = true;
        cv.notify_one();
    }
}

impl Drop for ClientWriter {
    fn drop(&mut self) {
        self.close();
    }
}

struct ClientState {
    id: ClientId,
    cwd: PathBuf,
    overrides: ClientEnvOverrides,
    is_cli: bool,
    tx: ClientWriter,
    workspace_root: Option<PathBuf>,
    workspace_uri: Option<String>,
    extra_folders: HashMap<PathBuf, String>,
    backend_key: Option<BackendKey>,
    backend_id: Option<BackendId>,
    init_options: Value,
    supports_progress: bool,
    initialized: bool,
    /// Active means this client currently holds a lease on its backend + workspace.
    /// Dormant means either it hasn't sent a file/query yet or it was hibernated.
    active: bool,
    last_activity: Instant,
    open_docs: HashMap<String, ClientDoc>,
    /// Requests queued while this client waits for `initialize` or backend wake.
    waiting_init_req: Option<Value>,
}

impl ClientState {
    fn send(&self, msg: &Value) {
        self.tx.send(msg);
    }

    fn owns_folder(&self, path: &Path) -> bool {
        self.workspace_root.as_deref() == Some(path) || self.extra_folders.contains_key(path)
    }

    fn matches_uri(&self, uri: &str) -> bool {
        self.open_docs.contains_key(uri)
            || self
                .workspace_uri
                .as_deref()
                .is_some_and(|root| uri_in_root(uri, root))
            || self
                .extra_folders
                .values()
                .any(|root| uri_in_root(uri, root))
    }
}

struct BackendDoc {
    holders: HashSet<ClientId>,
    version: i64,
    text: String,
    digest: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CoalesceKey {
    workspace_root: Option<PathBuf>,
    method: String,
    params_json: String,
    target_fp: Option<FileFingerprint>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum FileFingerprint {
    Open([u8; 32]),
    Disk {
        mtime_ns: i128,
        ctime_ns: i128,
        len: u64,
        ino: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SyntaxCacheKey {
    method: String,
    uri: String,
    params_json: String,
    fingerprint: FileFingerprint,
}

struct RequestWaiter {
    client_id: ClientId,
    client_req_id: Value,
    cancelled: bool,
}

struct InFlightRequest {
    method: String,
    original_msg: Value,
    retries_left: u8,
    generation: u64,
    coalesce_key: Option<CoalesceKey>,
    syntax_key: Option<SyntaxCacheKey>,
    waiters: Vec<RequestWaiter>,
}

impl InFlightRequest {
    fn all_cancelled(&self) -> bool {
        self.waiters.iter().all(|w| w.cancelled)
    }

    fn has_active_client(&self, cid: ClientId) -> bool {
        self.waiters
            .iter()
            .any(|w| w.client_id == cid && !w.cancelled)
    }
}

struct QueuedReadyMsg {
    client_id: ClientId,
    workspace_root: PathBuf,
    msg: Value,
}

struct BackendState {
    id: BackendId,
    key: BackendKey,
    child: Option<Child>,
    pid: u32,
    cancelled: Arc<AtomicBool>,
    setup_handle: Option<thread::JoinHandle<()>>,
    tx: Sender<Vec<u8>>,
    target_dir: Option<PathBuf>,
    ra_settings: Value,
    cache_backed: bool,
    check_via_jato: bool,
    seeded_files: usize,
    cli_warmed: bool,
    init_result: Option<Value>,
    ready: bool,
    quiescent: bool,
    generation: u64,
    loaded_folders: HashMap<PathBuf, FolderState>,
    /// Sibling worktree folders queued so startup and warm bursts send
    /// one coalesced `workspace/didChangeWorkspaceFolders` notification.
    pending_added_folders: Vec<(PathBuf, String)>,
    folder_flush_scheduled: bool,
    open_docs: HashMap<String, BackendDoc>,
    next_req_id: u64,
    next_retry_token: u64,
    in_flight: HashMap<u64, InFlightRequest>,
    /// Requests waiting for a `-32801` backoff retry (visible to cancel/evict/exit).
    retrying: HashMap<u64, InFlightRequest>,
    /// Bounded 100%-sound single-file syntax response cache (`documentSymbol`,
    /// `foldingRange`, `selectionRange`) keyed by content/stat fingerprint.
    syntax_cache: HashMap<SyntaxCacheKey, Value>,
    syntax_cache_order: VecDeque<SyntaxCacheKey>,
    /// Raw already-transformed frames (didOpen/didChange/didClose/didChangeWorkspaceFolders)
    /// queued while waiting for the backend's `initialize` response.
    pending_raw: VecDeque<Value>,
    /// Client messages queued while waiting for the backend's `initialize` response.
    pending_init: VecDeque<(ClientId, Value)>,
    /// Semantic queries held briefly while a newly-added worktree finishes `cargo metadata`.
    pending_folder_ready: VecDeque<QueuedReadyMsg>,
    last_activity: Instant,
    rss_bytes: u64,
}

struct FolderState {
    uri: String,
    added_at: Instant,
    /// False while queued in `pending_added_folders` awaiting batched send.
    sent: bool,
    saw_busy: bool,
    ready: bool,
    last_active: Instant,
}

impl FolderState {
    fn is_ready_or_timed_out(&self) -> bool {
        if self.ready {
            return true;
        }
        if !self.sent {
            return false;
        }
        let limit = if self.saw_busy {
            WORKSPACE_BUSY_READY_WAIT
        } else {
            WORKSPACE_READY_WAIT
        };
        self.added_at.elapsed() >= limit
    }
}

impl BackendState {
    fn send(&self, msg: &Value) {
        let _ = self.tx.send(encode_lsp_message(msg));
    }

    fn other_root_uris(&self, client: Option<&ClientState>) -> Vec<String> {
        self.loaded_folders
            .iter()
            .filter(|(p, _)| !client.is_some_and(|c| c.owns_folder(p)))
            .map(|(_, f)| f.uri.clone())
            .collect()
    }

    fn file_fingerprint(&self, uri: &str) -> Option<FileFingerprint> {
        if let Some(doc) = self.open_docs.get(uri) {
            return Some(FileFingerprint::Open(doc.digest));
        }
        let path = uri_to_path(uri)?;
        let meta = fs::metadata(&path).ok()?;
        use std::os::unix::fs::MetadataExt;
        Some(FileFingerprint::Disk {
            mtime_ns: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
            ctime_ns: i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec()),
            len: meta.len(),
            ino: meta.ino(),
        })
    }

    fn syntax_cache_key(&self, method: &str, msg: &Value) -> Option<SyntaxCacheKey> {
        if !matches!(
            method,
            "textDocument/documentSymbol"
                | "textDocument/foldingRange"
                | "textDocument/selectionRange"
        ) {
            return None;
        }
        let uri = msg
            .pointer("/params/textDocument/uri")
            .and_then(Value::as_str)?;
        let fingerprint = self.file_fingerprint(uri)?;
        let params_json = serde_json::to_string(msg.get("params").unwrap_or(&Value::Null)).ok()?;
        Some(SyntaxCacheKey {
            method: method.to_owned(),
            uri: uri.to_owned(),
            params_json,
            fingerprint,
        })
    }

    fn insert_syntax_cache(&mut self, key: SyntaxCacheKey, result: Value) {
        if let Some(slot) = self.syntax_cache.get_mut(&key) {
            *slot = result;
            return;
        }
        while self.syntax_cache.len() >= SYNTAX_CACHE_MAX {
            if let Some(oldest) = self.syntax_cache_order.pop_front() {
                self.syntax_cache.remove(&oldest);
            } else {
                break;
            }
        }
        self.syntax_cache_order.push_back(key.clone());
        self.syntax_cache.insert(key, result);
    }
}

fn is_coalesceable_read_method(method: &str) -> bool {
    matches!(
        method,
        "textDocument/hover"
            | "textDocument/definition"
            | "textDocument/typeDefinition"
            | "textDocument/declaration"
            | "textDocument/implementation"
            | "textDocument/references"
            | "textDocument/documentSymbol"
            | "textDocument/foldingRange"
            | "textDocument/selectionRange"
            | "textDocument/diagnostic"
            | "workspace/symbol"
    )
}

enum DaemonEvent {
    NewLspClient {
        cwd: PathBuf,
        overrides: ClientEnvOverrides,
        cli: bool,
        stream: UnixStream,
    },
    StatusRequest(UnixStream),
    StopRequest(UnixStream),
    ClientMessage(ClientId, Value),
    ClientDisconnected(ClientId),
    CapsProbed {
        ra_bin: PathBuf,
        result: Result<Value, String>,
    },
    BackendSpawned {
        bid: BackendId,
        child: ManagedChild,
        seeded_files: usize,
    },
    BackendMessage(BackendId, Value),
    BackendExited(BackendId),
    FireRetry(BackendId, u64),
    FlushAddedFolders(BackendId),
    Tick,
    SignalStop,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ToolchainCacheKey {
    workspace_root: PathBuf,
    rustup_toolchain: Option<String>,
    explicit_ra: Option<PathBuf>,
}

struct CachedToolchain {
    ra_bin: PathBuf,
    rustc_bin: PathBuf,
    stamp: (Option<FileFingerprint>, Option<FileFingerprint>),
    cached_at: Instant,
}

fn path_stat_fingerprint(path: &Path) -> Option<FileFingerprint> {
    let meta = fs::metadata(path).ok()?;
    use std::os::unix::fs::MetadataExt;
    Some(FileFingerprint::Disk {
        mtime_ns: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
        ctime_ns: i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec()),
        len: meta.len(),
        ino: meta.ino(),
    })
}

fn toolchain_file_stamp(ws: &Path) -> (Option<FileFingerprint>, Option<FileFingerprint>) {
    (
        path_stat_fingerprint(&ws.join("rust-toolchain.toml")),
        path_stat_fingerprint(&ws.join("rust-toolchain")),
    )
}

struct Daemon {
    cfg: DaemonConfig,
    tx: Sender<DaemonEvent>,
    started_at: Instant,
    next_client_id: ClientId,
    next_backend_id: BackendId,
    clients: HashMap<ClientId, ClientState>,
    backends: HashMap<BackendId, BackendState>,
    backend_by_key: HashMap<BackendKey, BackendId>,
    caps_cache: HashMap<PathBuf, Value>,
    /// Coalesces concurrent cold capability probes per `rust-analyzer` binary.
    probing_caps: HashMap<PathBuf, Vec<ClientId>>,
    /// Avoids spawning `rustup which` on every client `initialize`.
    toolchain_cache: HashMap<ToolchainCacheKey, CachedToolchain>,
    empty_since: Option<Instant>,
    backends_reused: u64,
    backends_hibernated: u64,
    backends_evicted: u64,
    requests_coalesced: u64,
    cache_hits: u64,
    backends_seeded: u64,
    seeded_files_total: u64,
    cli_queries: u64,
    tick_count: u64,
    shutdown: bool,
    stop_reply: Option<UnixStream>,
}

impl Daemon {
    fn new(cfg: DaemonConfig, tx: Sender<DaemonEvent>) -> Self {
        Self {
            cfg,
            tx,
            started_at: Instant::now(),
            next_client_id: 1,
            next_backend_id: 1,
            clients: HashMap::new(),
            backends: HashMap::new(),
            backend_by_key: HashMap::new(),
            caps_cache: HashMap::new(),
            probing_caps: HashMap::new(),
            toolchain_cache: HashMap::new(),
            empty_since: None,
            backends_reused: 0,
            backends_hibernated: 0,
            backends_evicted: 0,
            requests_coalesced: 0,
            cache_hits: 0,
            backends_seeded: 0,
            seeded_files_total: 0,
            cli_queries: 0,
            tick_count: 0,
            shutdown: false,
            stop_reply: None,
        }
    }

    fn handle(&mut self, ev: DaemonEvent) {
        match ev {
            DaemonEvent::NewLspClient {
                cwd,
                overrides,
                cli,
                stream,
            } => self.on_new_client(cwd, overrides, cli, stream),
            DaemonEvent::StatusRequest(stream) => self.on_status(stream),
            DaemonEvent::StopRequest(stream) => {
                self.stop_reply = Some(stream);
                self.shutdown = true;
            }
            DaemonEvent::ClientMessage(cid, msg) => self.on_client_message(cid, msg),
            DaemonEvent::ClientDisconnected(cid) => self.on_client_disconnected(cid),
            DaemonEvent::CapsProbed { ra_bin, result } => self.on_caps_probed(ra_bin, result),
            DaemonEvent::BackendSpawned {
                bid,
                child,
                seeded_files,
            } => self.on_backend_spawned(bid, child, seeded_files),
            DaemonEvent::BackendMessage(bid, msg) => self.on_backend_message(bid, msg),
            DaemonEvent::BackendExited(bid) => self.on_backend_exited(bid),
            DaemonEvent::FireRetry(bid, token) => self.on_fire_retry(bid, token),
            DaemonEvent::FlushAddedFolders(bid) => self.flush_added_folders(bid),
            DaemonEvent::Tick => self.on_tick(),
            DaemonEvent::SignalStop => {
                self.shutdown = true;
            }
        }
    }

    fn resolve_toolchain(
        &mut self,
        workspace_root: &Path,
        overrides: &ClientEnvOverrides,
    ) -> Result<(PathBuf, PathBuf)> {
        let key = ToolchainCacheKey {
            workspace_root: workspace_root.to_path_buf(),
            rustup_toolchain: overrides.rustup_toolchain.clone(),
            explicit_ra: overrides.rust_analyzer_bin.clone(),
        };
        let stamp = toolchain_file_stamp(workspace_root);
        if let Some(entry) = self.toolchain_cache.get(&key)
            && entry.stamp == stamp
            && entry.cached_at.elapsed() < TOOLCHAIN_CACHE_TTL
        {
            return Ok((entry.ra_bin.clone(), entry.rustc_bin.clone()));
        }
        let tc = overrides.rustup_toolchain.as_deref();
        let ra_bin = if let Some(explicit) = overrides.rust_analyzer_bin.as_deref() {
            fs::canonicalize(explicit).unwrap_or_else(|_| explicit.to_path_buf())
        } else {
            resolve_real_ra_for(workspace_root, tc)?
        };
        let rustc_bin = resolve_rustc_for(workspace_root, tc);
        self.toolchain_cache.insert(
            key,
            CachedToolchain {
                ra_bin: ra_bin.clone(),
                rustc_bin: rustc_bin.clone(),
                stamp,
                cached_at: Instant::now(),
            },
        );
        Ok((ra_bin, rustc_bin))
    }

    fn on_new_client(
        &mut self,
        cwd: PathBuf,
        overrides: ClientEnvOverrides,
        cli: bool,
        mut stream: UnixStream,
    ) {
        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
        if wire::send(&mut stream, br#"{"ok":true}"#).is_err() {
            return;
        }
        let _ = stream.set_write_timeout(None);
        self.empty_since = None;
        let id = self.next_client_id;
        self.next_client_id += 1;

        let Ok(read_stream) = stream.try_clone() else {
            return;
        };
        let out_tx = ClientWriter::spawn(stream);

        let ev_tx = self.tx.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(read_stream);
            while let Ok(Some(msg)) = read_lsp_message(&mut reader) {
                if ev_tx.send(DaemonEvent::ClientMessage(id, msg)).is_err() {
                    break;
                }
            }
            let _ = ev_tx.send(DaemonEvent::ClientDisconnected(id));
        });

        self.clients.insert(
            id,
            ClientState {
                id,
                cwd,
                overrides,
                is_cli: cli,
                tx: out_tx,
                workspace_root: None,
                workspace_uri: None,
                extra_folders: HashMap::new(),
                backend_key: None,
                backend_id: None,
                init_options: Value::Object(Map::new()),
                supports_progress: false,
                initialized: false,
                active: false,
                last_activity: Instant::now(),
                open_docs: HashMap::new(),
                waiting_init_req: None,
            },
        );
    }

    fn on_status(&mut self, mut stream: UnixStream) {
        self.refresh_rss();
        let now = Instant::now();
        let active_clients = self.clients.values().filter(|c| c.active).count();
        let mut backends = Vec::new();
        let mut total_rss = 0u64;
        for b in self.backends.values() {
            total_rss += b.rss_bytes;
            let count = self
                .clients
                .values()
                .filter(|c| c.active && c.backend_id == Some(b.id))
                .count();
            let mut workspaces: Vec<PathBuf> = b.loaded_folders.keys().cloned().collect();
            workspaces.sort();
            backends.push(BackendStatus {
                id: b.id,
                pid: b.pid,
                group: b.key.group.clone(),
                ra_bin: b.key.ra_bin.clone(),
                target_dir: b.target_dir.clone(),
                rss_bytes: b.rss_bytes,
                quiescent: b.quiescent,
                idle_secs: now.saturating_duration_since(b.last_activity).as_secs(),
                active_clients: count,
                open_docs: b.open_docs.len(),
                cache_backed: b.cache_backed,
                check_via_jato: b.check_via_jato,
                seeded_files: b.seeded_files,
                workspaces,
            });
        }
        backends.sort_by_key(|b| b.id);
        let status = DaemonStatus {
            pid: std::process::id(),
            uptime_secs: self.started_at.elapsed().as_secs(),
            memory_budget_bytes: self.cfg.memory_budget,
            max_backends: self.cfg.max_backends,
            idle_secs: self.cfg.idle_timeout.as_secs(),
            total_rss_bytes: total_rss,
            clients_connected: self.clients.len(),
            clients_active: active_clients,
            clients_dormant: self.clients.len().saturating_sub(active_clients),
            backends_reused: self.backends_reused,
            backends_hibernated: self.backends_hibernated,
            backends_evicted: self.backends_evicted,
            requests_coalesced: self.requests_coalesced,
            cache_hits: self.cache_hits,
            backends_seeded: self.backends_seeded,
            seeded_files_total: self.seeded_files_total,
            cli_queries: self.cli_queries,
            backends,
        };
        if let Ok(bytes) = serde_json::to_vec(&status) {
            let _ = wire::send(&mut stream, &bytes);
        }
    }

    fn on_client_message(&mut self, cid: ClientId, msg: Value) {
        let now = Instant::now();
        if let Some(c) = self.clients.get_mut(&cid) {
            c.last_activity = now;
        } else {
            return;
        }
        let method = msg.get("method").and_then(Value::as_str).map(str::to_owned);
        let has_id = msg.get("id").is_some();

        match method.as_deref() {
            Some("initialize") => {
                self.on_client_initialize(cid, msg);
                return;
            }
            Some("initialized") => {
                if let Some(c) = self.clients.get_mut(&cid) {
                    c.initialized = true;
                }
                return;
            }
            Some("shutdown") => {
                if let Some(req_id) = msg.get("id").cloned()
                    && let Some(c) = self.clients.get(&cid)
                {
                    c.send(&json!({ "jsonrpc": "2.0", "id": req_id, "result": null }));
                }
                self.detach_client_from_backend(cid);
                return;
            }
            Some("exit") => {
                self.on_client_disconnected(cid);
                return;
            }
            Some("$/cancelRequest") => {
                self.on_client_cancel(cid, &msg);
                return;
            }
            Some("$/setTrace") => return,
            // Client response to a server request (we handle server requests directly).
            None if has_id => return,
            _ => (),
        }

        // Reject requests sent before `initialize` per LSP spec (-32002).
        if self
            .clients
            .get(&cid)
            .is_some_and(|c| c.backend_key.is_none())
        {
            if let Some(req_id) = msg.get("id").cloned()
                && let Some(c) = self.clients.get(&cid)
            {
                c.send(&json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": { "code": -32002, "message": "server not initialized" }
                }));
            }
            return;
        }

        // Update client's open document shadow state before forwarding so
        // hibernation/wake and multi-client refcounting always stay accurate.
        self.update_client_doc_shadow(cid, method.as_deref(), &msg);

        // `didClose` on a dormant client needs no backend wake.
        if method.as_deref() == Some("textDocument/didClose")
            && self.clients.get(&cid).is_some_and(|c| !c.active)
        {
            return;
        }

        // Ensure backend is running and this client's workspace is attached.
        let Some((bid, just_woke)) = self.ensure_client_active(cid) else {
            if let Some(req_id) = msg.get("id").cloned()
                && let Some(c) = self.clients.get(&cid)
            {
                c.send(&json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": { "code": -32603, "message": "failed to start rust-analyzer backend" }
                }));
            }
            return;
        };

        // When waking a dormant client, `replay_client_docs` has already synced
        // the up-to-date `didOpen`/`didChange` buffer to the backend — do not
        // double-apply incremental edits.
        if just_woke
            && matches!(
                method.as_deref(),
                Some("textDocument/didOpen" | "textDocument/didChange")
            )
        {
            return;
        }

        if self.clients.get(&cid).is_some_and(|c| c.is_cli) && has_id {
            self.cli_queries += 1;
        }

        self.dispatch_to_backend(bid, cid, msg);
    }

    fn on_client_initialize(&mut self, cid: ClientId, msg: Value) {
        let Some(req_id) = msg.get("id").cloned() else {
            return;
        };
        if self
            .clients
            .get(&cid)
            .is_some_and(|c| c.backend_key.is_some())
        {
            if let Some(c) = self.clients.get(&cid) {
                c.send(&json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": { "code": -32600, "message": "server already initialized" }
                }));
            }
            return;
        }
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let (cwd, overrides) = {
            let Some(c) = self.clients.get(&cid) else {
                return;
            };
            (c.cwd.clone(), c.overrides.clone())
        };

        let raw_root = params
            .pointer("/workspaceFolders/0/uri")
            .and_then(Value::as_str)
            .and_then(uri_to_path)
            .or_else(|| {
                params
                    .get("rootUri")
                    .and_then(Value::as_str)
                    .and_then(uri_to_path)
            })
            .or_else(|| {
                params
                    .get("rootPath")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
            })
            .unwrap_or_else(|| cwd.clone());

        let canon_root = fs::canonicalize(&raw_root).unwrap_or(raw_root);
        let workspace_root = roots::workspace_above(&canon_root)
            .and_then(|p| fs::canonicalize(&p).ok().or(Some(p)))
            .unwrap_or(canon_root);
        let workspace_uri = path_to_uri(&workspace_root);

        let (ra_bin, rustc_bin) = match self.resolve_toolchain(&workspace_root, &overrides) {
            Ok(pair) => pair,
            Err(e) => {
                if let Some(c) = self.clients.get(&cid) {
                    c.send(&json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "error": { "code": -32603, "message": format!("{e:#}") }
                    }));
                }
                return;
            }
        };
        let share_worktrees = overrides
            .share_worktrees
            .unwrap_or(self.cfg.share_worktrees);
        let group = if share_worktrees {
            git_common_dir(&workspace_root).unwrap_or_else(|| workspace_root.clone())
        } else {
            workspace_root.clone()
        };
        let init_options = params
            .get("initializationOptions")
            .cloned()
            .filter(Value::is_object)
            .unwrap_or_else(|| Value::Object(Map::new()));
        let (eff_settings, eff_cfg) = self.cfg.merged_ra_settings(
            &group,
            &rustc_bin,
            &workspace_root,
            &init_options,
            &overrides,
        );
        let settings_hash = settings_fingerprint(&eff_settings, &eff_cfg);
        let key = BackendKey {
            group,
            ra_bin: ra_bin.clone(),
            rustc_bin,
            settings_hash,
        };
        let supports_progress = params
            .pointer("/capabilities/window/workDoneProgress")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let lazy = overrides.lazy.unwrap_or(self.cfg.lazy);

        let mut extra_folders = HashMap::new();
        if let Some(folders) = params.get("workspaceFolders").and_then(Value::as_array) {
            for f in folders.iter().skip(1) {
                if let Some(p) = f.get("uri").and_then(Value::as_str).and_then(uri_to_path) {
                    let canon = fs::canonicalize(&p).unwrap_or(p);
                    let root = roots::workspace_above(&canon)
                        .and_then(|r| fs::canonicalize(&r).ok().or(Some(r)))
                        .unwrap_or(canon);
                    if root != workspace_root {
                        let uri = path_to_uri(&root);
                        extra_folders.insert(root, uri);
                    }
                }
            }
        }

        if let Some(c) = self.clients.get_mut(&cid) {
            c.workspace_root = Some(workspace_root);
            c.workspace_uri = Some(workspace_uri);
            c.extra_folders = extra_folders;
            c.backend_key = Some(key.clone());
            c.init_options = init_options;
            c.supports_progress = supports_progress;
        }

        // Fast path 1: an active backend for this key already has InitializeResult!
        if let Some(&bid) = self.backend_by_key.get(&key)
            && let Some(res) = self.backends.get(&bid).and_then(|b| b.init_result.clone())
        {
            self.caps_cache.insert(ra_bin, res.clone());
            if !lazy {
                let _ = self.ensure_client_active(cid);
            }
            if let Some(c) = self.clients.get(&cid) {
                c.send(&json!({ "jsonrpc": "2.0", "id": req_id, "result": res }));
            }
            return;
        }

        // Fast path 2: lazy mode (default) — answer initialize immediately with
        // cached capabilities and keep RAM at 0 B until first file open/query!
        if lazy {
            if let Some(res) = self.caps_cache.get(&ra_bin).cloned() {
                if let Some(c) = self.clients.get(&cid) {
                    c.send(&json!({ "jsonrpc": "2.0", "id": req_id, "result": res }));
                }
                return;
            }
            if let Some(c) = self.clients.get_mut(&cid) {
                c.waiting_init_req = Some(req_id);
            }
            let already_probing = if let Some(waiters) = self.probing_caps.get_mut(&ra_bin) {
                waiters.push(cid);
                true
            } else {
                self.probing_caps.insert(ra_bin.clone(), vec![cid]);
                false
            };
            if !already_probing {
                let cache = self.cfg.cache_dir.clone();
                let tx = self.tx.clone();
                thread::spawn(move || {
                    let result =
                        probe_ra_capabilities(&cache, &ra_bin, cid).map_err(|e| format!("{e:#}"));
                    let _ = tx.send(DaemonEvent::CapsProbed { ra_bin, result });
                });
            }
            return;
        }

        // Eager mode (`JATO_RA_LAZY=0`): start backend right away and reply once initialized.
        if let Some(c) = self.clients.get_mut(&cid) {
            c.waiting_init_req = Some(req_id);
        }
        if self.ensure_client_active(cid).is_none()
            && let Some(c) = self.clients.get_mut(&cid)
            && let Some(req_id) = c.waiting_init_req.take()
        {
            c.send(&json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "error": { "code": -32603, "message": "failed to start rust-analyzer backend" }
            }));
        }
    }

    fn on_caps_probed(&mut self, ra_bin: PathBuf, result: Result<Value, String>) {
        let waiters = self.probing_caps.remove(&ra_bin).unwrap_or_default();
        match result {
            Ok(caps) => {
                self.caps_cache.insert(ra_bin, caps.clone());
                for cid in waiters {
                    if let Some(c) = self.clients.get_mut(&cid)
                        && let Some(req_id) = c.waiting_init_req.take()
                    {
                        c.send(&json!({ "jsonrpc": "2.0", "id": req_id, "result": caps }));
                    }
                }
            }
            Err(err) => {
                for cid in waiters {
                    if let Some(c) = self.clients.get_mut(&cid)
                        && let Some(req_id) = c.waiting_init_req.take()
                    {
                        c.send(&json!({
                            "jsonrpc": "2.0",
                            "id": req_id,
                            "error": { "code": -32603, "message": err }
                        }));
                    }
                }
            }
        }
    }

    fn update_client_doc_shadow(&mut self, cid: ClientId, method: Option<&str>, msg: &Value) {
        let Some(c) = self.clients.get_mut(&cid) else {
            return;
        };
        match method {
            Some("textDocument/didOpen") => {
                if let (Some(uri), Some(text)) = (
                    msg.pointer("/params/textDocument/uri")
                        .and_then(Value::as_str),
                    msg.pointer("/params/textDocument/text")
                        .and_then(Value::as_str),
                ) {
                    let language_id = msg
                        .pointer("/params/textDocument/languageId")
                        .and_then(Value::as_str)
                        .unwrap_or("rust")
                        .to_owned();
                    c.open_docs.insert(
                        uri.to_owned(),
                        ClientDoc {
                            language_id,
                            text: text.to_owned(),
                        },
                    );
                }
            }
            Some("textDocument/didChange") => {
                if let (Some(uri), Some(changes)) = (
                    msg.pointer("/params/textDocument/uri")
                        .and_then(Value::as_str),
                    msg.pointer("/params/contentChanges")
                        .and_then(Value::as_array),
                ) && let Some(doc) = c.open_docs.get_mut(uri)
                {
                    apply_content_changes(&mut doc.text, changes);
                }
            }
            Some("textDocument/didClose") => {
                if let Some(uri) = msg
                    .pointer("/params/textDocument/uri")
                    .and_then(Value::as_str)
                {
                    c.open_docs.remove(uri);
                }
            }
            _ => (),
        }
    }

    fn ensure_client_active(&mut self, cid: ClientId) -> Option<(BackendId, bool)> {
        let (key, ws_root, ws_uri, extra_folders, init_opts, overrides, was_active, existing_bid) = {
            let c = self.clients.get(&cid)?;
            (
                c.backend_key.clone()?,
                c.workspace_root.clone()?,
                c.workspace_uri.clone()?,
                c.extra_folders.clone(),
                c.init_options.clone(),
                c.overrides.clone(),
                c.active,
                c.backend_id,
            )
        };
        if was_active
            && let Some(bid) = existing_bid
            && self.backends.contains_key(&bid)
        {
            if let Some(b) = self.backends.get_mut(&bid) {
                let now = Instant::now();
                b.last_activity = now;
                if let Some(f) = b.loaded_folders.get_mut(&ws_root) {
                    f.last_active = now;
                }
                for ef in extra_folders.keys() {
                    if let Some(f) = b.loaded_folders.get_mut(ef) {
                        f.last_active = now;
                    }
                }
            }
            return Some((bid, false));
        }

        // Reuse existing backend for this repo group, or spawn a new one.
        let (bid, spawned) = if let Some(&bid) = self.backend_by_key.get(&key) {
            self.backends_reused += 1;
            (bid, false)
        } else {
            self.enforce_capacity_before_spawn();
            let bid = self.spawn_backend(key.clone(), &ws_root, &ws_uri, &init_opts, &overrides)?;
            (bid, true)
        };

        let is_cli = self.clients.get(&cid).is_some_and(|c| c.is_cli);
        if let Some(c) = self.clients.get_mut(&cid) {
            c.backend_id = Some(bid);
            c.active = true;
        }
        if is_cli && let Some(b) = self.backends.get_mut(&bid) {
            b.cli_warmed = true;
        }

        // Ensure this client's primary workspace (when reusing) and any extra
        // workspace folders are loaded in the shared backend.
        let mut folders_to_ensure: Vec<(PathBuf, String)> = Vec::new();
        if !spawned {
            folders_to_ensure.push((ws_root, ws_uri));
        }
        folders_to_ensure.extend(extra_folders);
        self.queue_backend_folders(bid, folders_to_ensure);

        // Replay any open documents this dormant/hibernated client already holds
        // (both when attaching to an existing backend and when waking a fresh one).
        self.replay_client_docs(bid, cid);
        Some((bid, true))
    }

    fn queue_backend_folders(&mut self, bid: BackendId, folders: Vec<(PathBuf, String)>) {
        let schedule_flush = {
            let Some(b) = self.backends.get_mut(&bid) else {
                return;
            };
            let now = Instant::now();
            let mut added_any = false;
            for (path, uri) in folders {
                if !b.loaded_folders.contains_key(&path) {
                    b.loaded_folders.insert(
                        path.clone(),
                        FolderState {
                            uri: uri.clone(),
                            added_at: now,
                            sent: false,
                            saw_busy: false,
                            ready: false,
                            last_active: now,
                        },
                    );
                    b.pending_added_folders.push((path, uri));
                    b.generation += 1;
                    b.quiescent = false;
                    b.last_activity = now;
                    added_any = true;
                }
            }
            if added_any && b.ready && !b.folder_flush_scheduled {
                b.folder_flush_scheduled = true;
                true
            } else {
                false
            }
        };
        if schedule_flush {
            let tx = self.tx.clone();
            thread::spawn(move || {
                thread::sleep(FOLDER_BATCH_DELAY);
                let _ = tx.send(DaemonEvent::FlushAddedFolders(bid));
            });
        }
    }

    fn flush_added_folders(&mut self, bid: BackendId) {
        let Some(b) = self.backends.get_mut(&bid) else {
            return;
        };
        b.folder_flush_scheduled = false;
        if !b.ready || b.pending_added_folders.is_empty() {
            return;
        }
        let now = Instant::now();
        let added: Vec<Value> = b
            .pending_added_folders
            .drain(..)
            .map(|(p, u)| {
                if let Some(f) = b.loaded_folders.get_mut(&p) {
                    f.sent = true;
                    f.added_at = now;
                    f.saw_busy = false;
                    f.ready = false;
                }
                let name = p.file_name().and_then(OsStr::to_str).unwrap_or("workspace");
                json!({ "uri": u, "name": name })
            })
            .collect();
        b.quiescent = false;
        b.send(&json!({
            "jsonrpc": "2.0",
            "method": "workspace/didChangeWorkspaceFolders",
            "params": { "event": { "added": added, "removed": [] } }
        }));
    }

    fn spawn_backend(
        &mut self,
        key: BackendKey,
        initial_ws: &Path,
        initial_uri: &str,
        client_opts: &Value,
        overrides: &ClientEnvOverrides,
    ) -> Option<BackendId> {
        let bid = self.next_backend_id;
        self.next_backend_id += 1;

        let (ra_settings, effective) = self.cfg.merged_ra_settings(
            &key.group,
            &key.rustc_bin,
            initial_ws,
            client_opts,
            overrides,
        );
        let target_dir = effective.target_dir.clone();

        let log_path = self
            .cfg
            .cache_dir
            .join("rust-analyzer")
            .join(format!("backend-{bid}.log"));

        let mut cmd = Command::new(&key.ra_bin);
        cmd.current_dir(initial_ws)
            .env("JATO_RA_PASSTHROUGH", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .process_group(0);
        if env::var_os("MALLOC_ARENA_MAX").is_none() {
            cmd.env("MALLOC_ARENA_MAX", "2");
        }
        if effective.cache_backed
            && let Some(exe) = own_jato_exe()
        {
            cmd.env("RUSTC_WRAPPER", &exe)
                .env("JATO_CACHE_DIR", &self.cfg.cache_dir);
        } else if env::var_os("RUSTC_WRAPPER").is_some_and(|w| is_self(Path::new(&w))) {
            cmd.env_remove("RUSTC_WRAPPER");
        }
        if let Some(tc) = overrides.rustup_toolchain.as_deref() {
            cmd.env("RUSTUP_TOOLCHAIN", tc);
        } else {
            cmd.env_remove("RUSTUP_TOOLCHAIN");
        }

        let (out_tx, out_rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let setup_cancelled = Arc::clone(&cancelled);
        let ev_tx = self.tx.clone();
        let cache_dir = self.cfg.cache_dir.clone();
        let ws_for_seed = initial_ws.to_path_buf();
        let target_for_seed = target_dir.clone();
        let check_target_to_create = effective.check_target_dir.clone();
        let seed_enabled = effective.seed;
        let rustup_tc = overrides.rustup_toolchain.clone();
        let cargo_extra_env = effective.cargo_extra_env.clone();
        let ra_bin_for_log = key.ra_bin.clone();

        struct SetupExitGuard {
            ev_tx: Sender<DaemonEvent>,
            bid: BackendId,
            armed: bool,
        }
        impl Drop for SetupExitGuard {
            fn drop(&mut self) {
                if self.armed {
                    let _ = self.ev_tx.send(DaemonEvent::BackendExited(self.bid));
                }
            }
        }

        let setup_handle = thread::spawn(move || {
            let mut exit_guard = SetupExitGuard {
                ev_tx: ev_tx.clone(),
                bid,
                armed: true,
            };
            if setup_cancelled.load(Ordering::Acquire) {
                exit_guard.armed = false;
                return;
            }
            let mut seeded_files = 0usize;
            if let Some(ref tdir) = target_for_seed {
                if seed_enabled {
                    match crate::command::seed_target_for_workspace(
                        &cache_dir,
                        &ws_for_seed,
                        tdir,
                        rustup_tc.as_deref(),
                        &cargo_extra_env,
                        &setup_cancelled,
                    ) {
                        Ok(n) => seeded_files = n,
                        Err(e) => {
                            eprintln!("jato rust-analyzer: pre-seed skipped: {e:#}");
                        }
                    }
                }
                let _ = fs::create_dir_all(tdir);
            }
            if let Some(ref check_dir) = check_target_to_create {
                let _ = fs::create_dir_all(check_dir);
            }
            if setup_cancelled.load(Ordering::Acquire) {
                exit_guard.armed = false;
                return;
            }

            let stderr = File::create(&log_path).map_or_else(|_| Stdio::null(), Stdio::from);
            cmd.stderr(stderr);
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!(
                        "jato rust-analyzer: failed to spawn {}: {e}",
                        ra_bin_for_log.display()
                    );
                    return;
                }
            };
            let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
                let _guard = ManagedChild::new(child);
                return;
            };
            let managed = ManagedChild::new(child);
            if setup_cancelled.load(Ordering::Acquire) {
                exit_guard.armed = false;
                drop(managed);
                return;
            }

            // Send `BackendSpawned` strictly before starting the stdout reader so
            // the daemon always receives `BackendSpawned` ahead of any `BackendMessage`
            // or `BackendExited` on the FIFO event channel. `ManagedChild` guarantees
            // `-pgrp` is killed and reaped even if `ev_tx` is dropped or unread.
            if ev_tx
                .send(DaemonEvent::BackendSpawned {
                    bid,
                    child: managed,
                    seeded_files,
                })
                .is_err()
            {
                exit_guard.armed = false;
                return;
            }
            exit_guard.armed = false;

            thread::spawn(move || {
                for bytes in out_rx {
                    if stdin.write_all(&bytes).is_err() || stdin.flush().is_err() {
                        break;
                    }
                }
            });

            let read_tx = ev_tx.clone();
            thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                while let Ok(Some(msg)) = read_lsp_message(&mut reader) {
                    if read_tx.send(DaemonEvent::BackendMessage(bid, msg)).is_err() {
                        break;
                    }
                }
                let _ = read_tx.send(DaemonEvent::BackendExited(bid));
            });
        });

        let now = Instant::now();
        let mut loaded_folders = HashMap::new();
        loaded_folders.insert(
            initial_ws.to_path_buf(),
            FolderState {
                uri: initial_uri.to_owned(),
                added_at: now,
                sent: true,
                saw_busy: false,
                ready: false,
                last_active: now,
            },
        );

        let name = initial_ws
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("workspace");
        // Note: intentionally do NOT advertise `textDocument.diagnostic` here.
        // Advertising pull diagnostics makes rust-analyzer suppress native
        // `textDocument/publishDiagnostics` pushes for push-only editors, while
        // rust-analyzer still answers explicit `textDocument/diagnostic` pull
        // requests from `jato diag` without it.
        let init_msg = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": initial_uri,
                "workspaceFolders": [{ "uri": initial_uri, "name": name }],
                "capabilities": {
                    "workspace": {
                        "workspaceFolders": true,
                        "configuration": true,
                        "symbol": { "dynamicRegistration": false }
                    },
                    "textDocument": {
                        "synchronization": { "didSave": true },
                        "hover": { "contentFormat": ["markdown", "plaintext"] },
                        "definition": { "linkSupport": true },
                        "typeDefinition": { "linkSupport": true },
                        "implementation": { "linkSupport": true },
                        "references": {},
                        "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                        "completion": { "completionItem": { "snippetSupport": true } },
                        "publishDiagnostics": {}
                    },
                    "window": { "workDoneProgress": true },
                    "experimental": { "serverStatusNotification": true }
                },
                "initializationOptions": ra_settings
            }
        });

        let backend = BackendState {
            id: bid,
            key: key.clone(),
            child: None,
            pid: 0,
            cancelled,
            setup_handle: Some(setup_handle),
            tx: out_tx,
            target_dir,
            ra_settings,
            cache_backed: effective.cache_backed,
            check_via_jato: effective.check_via_jato,
            seeded_files: 0,
            cli_warmed: false,
            init_result: None,
            ready: false,
            quiescent: false,
            generation: 0,
            loaded_folders,
            pending_added_folders: Vec::new(),
            folder_flush_scheduled: false,
            open_docs: HashMap::new(),
            next_req_id: 2,
            next_retry_token: 1,
            in_flight: HashMap::new(),
            retrying: HashMap::new(),
            syntax_cache: HashMap::new(),
            syntax_cache_order: VecDeque::new(),
            pending_raw: VecDeque::new(),
            pending_init: VecDeque::new(),
            pending_folder_ready: VecDeque::new(),
            last_activity: now,
            rss_bytes: 0,
        };
        backend.send(&init_msg);
        self.backends.insert(bid, backend);
        self.backend_by_key.insert(key, bid);
        Some(bid)
    }

    fn on_backend_spawned(
        &mut self,
        bid: BackendId,
        mut managed: ManagedChild,
        seeded_files: usize,
    ) {
        if let Some(b) = self.backends.get_mut(&bid) {
            let now = Instant::now();
            b.pid = managed.pid;
            b.child = managed.take();
            b.seeded_files = seeded_files;
            b.setup_handle = None;
            b.last_activity = now;
            for folder in b.loaded_folders.values_mut() {
                if !folder.ready {
                    folder.added_at = now;
                }
            }
            if seeded_files > 0 {
                self.backends_seeded += 1;
                self.seeded_files_total += seeded_files as u64;
            }
        }
        // If `bid` was stopped/evicted before its setup thread finished, dropping
        // `managed` here immediately SIGKILLs `-pgrp` and waits on the child.
    }

    fn replay_client_docs(&mut self, bid: BackendId, cid: ClientId) {
        let docs: Vec<(String, ClientDoc)> = self
            .clients
            .get(&cid)
            .map(|c| {
                c.open_docs
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default();
        for (uri, doc) in docs {
            self.sync_doc_open_to_backend(bid, cid, &uri, &doc.language_id, &doc.text);
        }
    }

    fn sync_doc_open_to_backend(
        &mut self,
        bid: BackendId,
        cid: ClientId,
        uri: &str,
        language_id: &str,
        text: &str,
    ) {
        let caller_is_cli = self.clients.get(&cid).is_some_and(|c| c.is_cli);
        let has_editor_holder = self
            .backends
            .get(&bid)
            .and_then(|b| b.open_docs.get(uri))
            .is_some_and(|existing| {
                existing
                    .holders
                    .iter()
                    .any(|&h| h != cid && self.clients.get(&h).is_some_and(|c| !c.is_cli))
            });
        let Some(b) = self.backends.get_mut(&bid) else {
            return;
        };
        if let Some(existing) = b.open_docs.get_mut(uri) {
            existing.holders.insert(cid);
            // Never let a transient CLI query overwrite an active editor's
            // unsaved buffer in the shared backend.
            if existing.text != text && !(caller_is_cli && has_editor_holder) {
                existing.version += 1;
                existing.text = text.to_owned();
                existing.digest = *blake3::hash(text.as_bytes()).as_bytes();
                b.generation += 1;
                let change = json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/didChange",
                    "params": {
                        "textDocument": { "uri": uri, "version": existing.version },
                        "contentChanges": [{ "text": text }]
                    }
                });
                if b.ready {
                    b.send(&change);
                } else {
                    b.pending_raw.push_back(change);
                }
            }
            return;
        }
        let mut holders = HashSet::new();
        holders.insert(cid);
        b.generation += 1;
        b.open_docs.insert(
            uri.to_owned(),
            BackendDoc {
                holders,
                version: 1,
                text: text.to_owned(),
                digest: *blake3::hash(text.as_bytes()).as_bytes(),
            },
        );
        let open = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": text
                }
            }
        });
        if b.ready {
            b.send(&open);
        } else {
            b.pending_raw.push_back(open);
        }
    }

    fn dispatch_to_backend(&mut self, bid: BackendId, cid: ClientId, mut msg: Value) {
        let method = msg.get("method").and_then(Value::as_str).map(str::to_owned);
        let client_root = self
            .clients
            .get(&cid)
            .and_then(|c| c.workspace_root.clone());

        // Document lifecycle messages use backend-side refcounting & monotonic versions.
        match method.as_deref() {
            Some("textDocument/didOpen") => {
                if let (Some(uri), Some(text)) = (
                    msg.pointer("/params/textDocument/uri")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    msg.pointer("/params/textDocument/text")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                ) {
                    let lang = msg
                        .pointer("/params/textDocument/languageId")
                        .and_then(Value::as_str)
                        .unwrap_or("rust")
                        .to_owned();
                    self.sync_doc_open_to_backend(bid, cid, &uri, &lang, &text);
                }
                return;
            }
            Some("textDocument/didChange") => {
                let Some(uri) = msg
                    .pointer("/params/textDocument/uri")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                else {
                    return;
                };
                let full_text = self
                    .clients
                    .get(&cid)
                    .and_then(|c| c.open_docs.get(&uri))
                    .map(|d| d.text.clone());
                if let Some(b) = self.backends.get_mut(&bid)
                    && let Some(doc) = b.open_docs.get_mut(&uri)
                {
                    doc.version += 1;
                    b.generation += 1;
                    let shared = doc.holders.len() > 1;
                    if let Some(full) = full_text {
                        doc.text = full;
                    } else if let Some(changes) = msg
                        .pointer("/params/contentChanges")
                        .and_then(Value::as_array)
                    {
                        apply_content_changes(&mut doc.text, changes);
                    }
                    doc.digest = *blake3::hash(doc.text.as_bytes()).as_bytes();
                    let forward = if shared {
                        json!({
                            "jsonrpc": "2.0",
                            "method": "textDocument/didChange",
                            "params": {
                                "textDocument": { "uri": uri, "version": doc.version },
                                "contentChanges": [{ "text": doc.text }]
                            }
                        })
                    } else {
                        if let Some(td) = msg.pointer_mut("/params/textDocument") {
                            td["version"] = json!(doc.version);
                        }
                        msg
                    };
                    if b.ready {
                        b.send(&forward);
                    } else {
                        b.pending_raw.push_back(forward);
                    }
                }
                return;
            }
            Some("textDocument/didClose") => {
                if let Some(uri) = msg
                    .pointer("/params/textDocument/uri")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                {
                    self.release_doc_holder(bid, cid, &uri);
                }
                return;
            }
            Some("workspace/didChangeWorkspaceFolders") => {
                // Never allow a single client to unload a folder still used by
                // other clients on the same shared backend; track per-client
                // extra folders and only add new folders safely.
                if let Some(removed) = msg
                    .pointer("/params/event/removed")
                    .and_then(Value::as_array)
                    && let Some(c) = self.clients.get_mut(&cid)
                {
                    for f in removed {
                        if let Some(p) = f.get("uri").and_then(Value::as_str).and_then(uri_to_path)
                        {
                            let canon = fs::canonicalize(&p).unwrap_or(p);
                            c.extra_folders.remove(&canon);
                        }
                    }
                }
                if let Some(added) = msg.pointer("/params/event/added").and_then(Value::as_array) {
                    let mut to_add = Vec::new();
                    for f in added {
                        let Some(uri) = f.get("uri").and_then(Value::as_str) else {
                            continue;
                        };
                        let Some(p) = uri_to_path(uri) else {
                            continue;
                        };
                        let canon = fs::canonicalize(&p).unwrap_or(p);
                        let norm_uri = path_to_uri(&canon);
                        if let Some(c) = self.clients.get_mut(&cid) {
                            c.extra_folders.insert(canon.clone(), norm_uri.clone());
                        }
                        to_add.push((canon, norm_uri));
                    }
                    self.queue_backend_folders(bid, to_add);
                }
                return;
            }
            Some(
                "textDocument/didSave"
                | "workspace/didChangeWatchedFiles"
                | "workspace/didChangeConfiguration",
            ) => {
                if let Some(b) = self.backends.get_mut(&bid) {
                    b.generation += 1;
                }
            }
            _ => (),
        }

        let Some(b) = self.backends.get_mut(&bid) else {
            return;
        };
        b.last_activity = Instant::now();
        if let Some(ws) = client_root.as_ref()
            && let Some(folder) = b.loaded_folders.get_mut(ws)
        {
            folder.last_active = Instant::now();
        }

        if !b.ready {
            b.pending_init.push_back((cid, msg));
            return;
        }

        // If this is a request for a workspace that was just added/started and
        // has not finished initial Cargo metadata load yet, queue it briefly so
        // the client gets real results instead of an early empty response.
        let is_cli_client = self.clients.get(&cid).is_some_and(|c| c.is_cli);
        if msg.get("id").is_some()
            && let Some(ws) = client_root.as_ref()
            && let Some(folder) = b.loaded_folders.get(ws)
        {
            if !folder.is_ready_or_timed_out() {
                b.pending_folder_ready.push_back(QueuedReadyMsg {
                    client_id: cid,
                    workspace_root: ws.clone(),
                    msg,
                });
                return;
            }
            // Never let `jato diag` silently return 0 errors when workspace
            // analysis timed out before becoming ready.
            if !folder.ready
                && is_cli_client
                && method.as_deref() == Some("textDocument/diagnostic")
                && let Some(req_id) = msg.get("id").cloned()
            {
                if let Some(c) = self.clients.get(&cid) {
                    c.send(&json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "error": {
                            "code": -32001,
                            "message": "rust-analyzer workspace analysis did not finish within ready timeout"
                        }
                    }));
                }
                return;
            }
        }

        self.send_request_or_notif(bid, cid, msg);
    }

    fn send_request_or_notif(&mut self, bid: BackendId, cid: ClientId, mut msg: Value) {
        let Some(client_req_id) = msg.get("id").cloned() else {
            if let Some(b) = self.backends.get(&bid) {
                b.send(&msg);
            }
            return;
        };
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let workspace_root = self
            .clients
            .get(&cid)
            .and_then(|c| c.workspace_root.clone());

        // Fast path 1: 100%-sound single-file syntax cache (keyed by content
        // digest for open buffers or mtime/ctime/len/ino for on-disk files).
        let syntax_key = self
            .backends
            .get(&bid)
            .and_then(|b| b.syntax_cache_key(&method, &msg));
        if let Some(ref sk) = syntax_key
            && let Some(cached) = self
                .backends
                .get(&bid)
                .and_then(|b| b.syntax_cache.get(sk))
                .cloned()
        {
            self.cache_hits += 1;
            let now = Instant::now();
            if let Some(b) = self.backends.get_mut(&bid) {
                b.last_activity = now;
            }
            if let Some(c) = self.clients.get_mut(&cid) {
                c.last_activity = now;
                c.send(&json!({
                    "jsonrpc": "2.0",
                    "id": client_req_id,
                    "result": cached
                }));
            }
            return;
        }

        // Fast path 2: Singleflight coalescing of concurrent identical
        // read-only requests on the same backend generation and file fingerprint.
        let coalesce_key = is_coalesceable_read_method(&method).then(|| {
            let params_json = serde_json::to_string(msg.get("params").unwrap_or(&Value::Null))
                .unwrap_or_default();
            let target_fp = msg
                .pointer("/params/textDocument/uri")
                .and_then(Value::as_str)
                .and_then(|u| self.backends.get(&bid).and_then(|b| b.file_fingerprint(u)));
            CoalesceKey {
                workspace_root,
                method: method.clone(),
                params_json,
                target_fp,
            }
        });
        if let Some(ref ck) = coalesce_key
            && let Some(b) = self.backends.get_mut(&bid)
        {
            let cur_gen = b.generation;
            let attached = b
                .in_flight
                .values_mut()
                .chain(b.retrying.values_mut())
                .find(|req| {
                    req.generation == cur_gen
                        && !req.all_cancelled()
                        && req.coalesce_key.as_ref() == Some(ck)
                });
            if let Some(existing) = attached {
                existing.waiters.push(RequestWaiter {
                    client_id: cid,
                    client_req_id,
                    cancelled: false,
                });
                self.requests_coalesced += 1;
                return;
            }
        }

        let Some(b) = self.backends.get_mut(&bid) else {
            return;
        };
        let backend_id = b.next_req_id;
        b.next_req_id += 1;
        let original_msg = msg.clone();
        msg["id"] = json!(backend_id);
        let generation = b.generation;
        b.in_flight.insert(
            backend_id,
            InFlightRequest {
                method,
                original_msg,
                retries_left: MAX_CONTENT_MODIFIED_RETRIES,
                generation,
                coalesce_key,
                syntax_key,
                waiters: vec![RequestWaiter {
                    client_id: cid,
                    client_req_id,
                    cancelled: false,
                }],
            },
        );
        b.send(&msg);
    }

    fn release_doc_holder(&mut self, bid: BackendId, cid: ClientId, uri: &str) {
        let remaining_holder = {
            let Some(b) = self.backends.get_mut(&bid) else {
                return;
            };
            match b.open_docs.get_mut(uri) {
                Some(doc) => {
                    doc.holders.remove(&cid);
                    if doc.holders.is_empty() {
                        b.open_docs.remove(uri);
                        b.generation += 1;
                        let close = json!({
                            "jsonrpc": "2.0",
                            "method": "textDocument/didClose",
                            "params": { "textDocument": { "uri": uri } }
                        });
                        if b.ready {
                            b.send(&close);
                        } else {
                            b.pending_raw.push_back(close);
                        }
                        return;
                    }
                    // Prefer non-CLI editor holders deterministically when restoring.
                    doc.holders
                        .iter()
                        .copied()
                        .min_by_key(|h| (self.clients.get(h).is_some_and(|c| c.is_cli), *h))
                }
                None => return,
            }
        };
        // If another client still holds this document open with different buffer
        // text, restore that remaining client's buffer to the backend.
        if let Some(other_cid) = remaining_holder
            && let Some(other_text) = self
                .clients
                .get(&other_cid)
                .and_then(|c| c.open_docs.get(uri))
                .map(|d| d.text.clone())
            && let Some(b) = self.backends.get_mut(&bid)
            && let Some(doc) = b.open_docs.get_mut(uri)
            && doc.text != other_text
        {
            doc.version += 1;
            doc.text = other_text.clone();
            doc.digest = *blake3::hash(other_text.as_bytes()).as_bytes();
            b.generation += 1;
            let restore = json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": {
                    "textDocument": { "uri": uri, "version": doc.version },
                    "contentChanges": [{ "text": other_text }]
                }
            });
            if b.ready {
                b.send(&restore);
            } else {
                b.pending_raw.push_back(restore);
            }
        }
    }

    fn on_client_cancel(&mut self, cid: ClientId, msg: &Value) {
        let Some(cancel_id) = msg.pointer("/params/id") else {
            return;
        };
        let Some(bid) = self.clients.get(&cid).and_then(|c| c.backend_id) else {
            return;
        };
        let Some(b) = self.backends.get_mut(&bid) else {
            return;
        };
        let mut to_cancel_backend = Vec::new();
        for (&backend_id, req) in &mut b.in_flight {
            for w in &mut req.waiters {
                if w.client_id == cid && &w.client_req_id == cancel_id {
                    w.cancelled = true;
                }
            }
            if req.all_cancelled() {
                to_cancel_backend.push(backend_id);
            }
        }
        for req in b.retrying.values_mut() {
            for w in &mut req.waiters {
                if w.client_id == cid && &w.client_req_id == cancel_id {
                    w.cancelled = true;
                }
            }
        }
        b.retrying.retain(|_, req| !req.all_cancelled());
        b.pending_folder_ready
            .retain(|q| !(q.client_id == cid && q.msg.get("id") == Some(cancel_id)));
        for backend_id in to_cancel_backend {
            b.send(&json!({
                "jsonrpc": "2.0",
                "method": "$/cancelRequest",
                "params": { "id": backend_id }
            }));
        }
    }

    fn detach_client_from_backend(&mut self, cid: ClientId) {
        let (bid, uris): (Option<BackendId>, Vec<String>) = match self.clients.get_mut(&cid) {
            Some(c) => {
                c.active = false;
                let bid = c.backend_id;
                let uris = c.open_docs.keys().cloned().collect();
                (bid, uris)
            }
            None => return,
        };
        if let Some(bid) = bid {
            for uri in uris {
                self.release_doc_holder(bid, cid, &uri);
            }
            if let Some(b) = self.backends.get_mut(&bid) {
                b.pending_folder_ready.retain(|q| q.client_id != cid);
                for req in b.in_flight.values_mut().chain(b.retrying.values_mut()) {
                    for w in &mut req.waiters {
                        if w.client_id == cid {
                            w.cancelled = true;
                        }
                    }
                }
                b.retrying.retain(|_, req| !req.all_cancelled());
            }
        }
    }

    fn on_client_disconnected(&mut self, cid: ClientId) {
        self.detach_client_from_backend(cid);
        self.clients.remove(&cid);
        if self.clients.is_empty() && self.backends.is_empty() {
            self.empty_since.get_or_insert_with(Instant::now);
        }
    }

    fn on_backend_message(&mut self, bid: BackendId, msg: Value) {
        let has_id = msg.get("id").is_some();
        let method = msg.get("method").and_then(Value::as_str).map(str::to_owned);

        // Case 1: Backend response to our `initialize` (id == 1) or a proxied client request.
        if has_id && method.is_none() {
            let Some(backend_req_id) = msg.get("id").and_then(Value::as_u64) else {
                return;
            };
            if backend_req_id == 1 {
                self.on_backend_initialized(bid, msg);
                return;
            }
            let Some(mut req) = self
                .backends
                .get_mut(&bid)
                .and_then(|b| b.in_flight.remove(&backend_req_id))
            else {
                return;
            };
            req.waiters
                .retain(|w| !w.cancelled && self.clients.contains_key(&w.client_id));
            if req.waiters.is_empty() {
                return;
            }
            // Retry transient `ContentModified` (-32801) or retriggerable LSP 3.17
            // `ServerCancelled` (-32802 with `data.retriggerRequest == true`).
            let err_code = msg.pointer("/error/code").and_then(Value::as_i64);
            let retrigger = err_code == Some(-32802)
                && msg.pointer("/error/data/retriggerRequest") == Some(&Value::Bool(true));
            if (matches!(err_code, Some(-32801)) || retrigger) && req.retries_left > 0 {
                req.retries_left -= 1;
                if let Some(b) = self.backends.get_mut(&bid) {
                    let token = b.next_retry_token;
                    b.next_retry_token += 1;
                    b.retrying.insert(token, req);
                    let tx = self.tx.clone();
                    thread::spawn(move || {
                        thread::sleep(RETRY_DELAY);
                        let _ = tx.send(DaemonEvent::FireRetry(bid, token));
                    });
                }
                return;
            }
            // Cache 100%-sound single-file syntax results when non-empty, no
            // error occurred, and the file fingerprint is unchanged.
            if msg.get("error").is_none()
                && let Some(ref sk) = req.syntax_key
                && let Some(res) = msg.get("result")
                && !res.is_null()
                && !res.as_array().is_some_and(Vec::is_empty)
                && let Some(b) = self.backends.get_mut(&bid)
                && b.file_fingerprint(&sk.uri).as_ref() == Some(&sk.fingerprint)
            {
                b.insert_syntax_cache(sk.clone(), res.clone());
            }

            let now = Instant::now();
            if let Some(b) = self.backends.get_mut(&bid) {
                b.last_activity = now;
            }
            for w in req.waiters {
                let mut reply = msg.clone();
                let other_roots = self
                    .backends
                    .get(&bid)
                    .map(|b| b.other_root_uris(self.clients.get(&w.client_id)))
                    .unwrap_or_default();
                if let Some(res) = reply.get_mut("result") {
                    filter_response_for_workspace(&req.method, res, &other_roots);
                }
                reply["id"] = w.client_req_id;
                if let Some(c) = self.clients.get_mut(&w.client_id) {
                    c.last_activity = now;
                    c.send(&reply);
                }
            }
            return;
        }

        // Case 2: Server-to-client request (`id` + `method`).
        if has_id && let Some(ref m) = method {
            let srv_id = msg.get("id").cloned().unwrap();
            let reply = if m == "workspace/configuration" {
                let cfg = self
                    .backends
                    .get(&bid)
                    .map(|b| b.ra_settings.clone())
                    .unwrap_or(Value::Null);
                let items = msg
                    .pointer("/params/items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let values: Vec<Value> = items
                    .iter()
                    .map(|item| config_section(&cfg, item.get("section").and_then(Value::as_str)))
                    .collect();
                json!({ "jsonrpc": "2.0", "id": srv_id, "result": values })
            } else {
                json!({ "jsonrpc": "2.0", "id": srv_id, "result": null })
            };
            if let Some(b) = self.backends.get(&bid) {
                b.send(&reply);
            }
            return;
        }

        // Case 3: Server-to-client notification.
        match method.as_deref() {
            Some("experimental/serverStatus") => {
                let quiescent = msg
                    .pointer("/params/quiescent")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let mut became_ready = false;
                if let Some(b) = self.backends.get_mut(&bid) {
                    let was_busy = !b.quiescent;
                    b.quiescent = quiescent;
                    if !quiescent {
                        for folder in b.loaded_folders.values_mut() {
                            if folder.sent && !folder.ready {
                                folder.saw_busy = true;
                            }
                        }
                    } else {
                        for folder in b.loaded_folders.values_mut() {
                            if folder.sent
                                && !folder.ready
                                && (folder.saw_busy
                                    || (was_busy
                                        && folder.added_at.elapsed() >= Duration::from_millis(250)))
                            {
                                folder.ready = true;
                                became_ready = true;
                            }
                        }
                    }
                }
                if became_ready {
                    self.flush_ready_queue(bid);
                }
            }
            Some("textDocument/publishDiagnostics") => {
                let uri = msg
                    .pointer("/params/uri")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                for c in self.clients.values() {
                    if !c.active || c.backend_id != Some(bid) {
                        continue;
                    }
                    if c.matches_uri(uri) {
                        c.send(&msg);
                    }
                }
            }
            Some("$/progress") => {
                if msg.pointer("/params/value/kind").and_then(Value::as_str) == Some("begin")
                    && let Some(b) = self.backends.get_mut(&bid)
                {
                    for folder in b.loaded_folders.values_mut() {
                        if folder.sent && !folder.ready {
                            folder.saw_busy = true;
                        }
                    }
                }
                for c in self.clients.values() {
                    if c.active && c.backend_id == Some(bid) && c.supports_progress {
                        c.send(&msg);
                    }
                }
            }
            _ => {
                for c in self.clients.values() {
                    if c.active && c.backend_id == Some(bid) {
                        c.send(&msg);
                    }
                }
            }
        }
    }

    fn on_backend_initialized(&mut self, bid: BackendId, msg: Value) {
        let Some(res) = msg.get("result").cloned() else {
            let err = msg.get("error").cloned().unwrap_or_else(|| {
                json!({ "code": -32603, "message": "rust-analyzer backend failed to initialize" })
            });
            let pending = self
                .backends
                .get_mut(&bid)
                .map(|b| b.pending_init.drain(..).collect::<Vec<_>>())
                .unwrap_or_default();
            for (cid, q) in pending {
                if let Some(req_id) = q.get("id").cloned()
                    && let Some(c) = self.clients.get(&cid)
                {
                    c.send(&json!({ "jsonrpc": "2.0", "id": req_id, "error": err }));
                }
            }
            for c in self.clients.values_mut() {
                if c.backend_id == Some(bid)
                    && let Some(req_id) = c.waiting_init_req.take()
                {
                    c.send(&json!({ "jsonrpc": "2.0", "id": req_id, "error": err }));
                }
            }
            self.stop_backend(bid, false);
            return;
        };
        let (ra_bin, pending_raw, pending) = {
            let Some(b) = self.backends.get_mut(&bid) else {
                return;
            };
            b.init_result = Some(res.clone());
            b.ready = true;
            b.send(&json!({
                "jsonrpc": "2.0",
                "method": "initialized",
                "params": {}
            }));
            let pending_raw: Vec<_> = b.pending_raw.drain(..).collect();
            let pending: Vec<_> = b.pending_init.drain(..).collect();
            (b.key.ra_bin.clone(), pending_raw, pending)
        };
        // Coalesce all worktree folders attached during backend startup
        // into ONE `workspace/didChangeWorkspaceFolders` notification before
        // replaying queued `didOpen` frames!
        self.flush_added_folders(bid);
        if let Some(b) = self.backends.get(&bid) {
            for raw in pending_raw {
                b.send(&raw);
            }
        }
        write_caps_cache(&caps_cache_path(&self.cfg.cache_dir, &ra_bin), &res, bid);
        self.caps_cache.insert(ra_bin, res.clone());

        // Reply to any eager clients waiting for `initialize`.
        let waiting: Vec<ClientId> = self
            .clients
            .values()
            .filter(|c| c.backend_id == Some(bid) && c.waiting_init_req.is_some())
            .map(|c| c.id)
            .collect();
        for cid in waiting {
            if let Some(c) = self.clients.get_mut(&cid)
                && let Some(req_id) = c.waiting_init_req.take()
            {
                c.send(&json!({ "jsonrpc": "2.0", "id": req_id, "result": res }));
            }
        }

        // Flush messages queued before backend initialization finished.
        for (cid, queued) in pending {
            if self.clients.contains_key(&cid) {
                self.dispatch_to_backend(bid, cid, queued);
            }
        }
    }

    fn flush_ready_queue(&mut self, bid: BackendId) {
        let queued: Vec<(QueuedReadyMsg, bool)> = {
            let Some(b) = self.backends.get_mut(&bid) else {
                return;
            };
            let mut ready = Vec::new();
            let mut keep = VecDeque::new();
            while let Some(item) = b.pending_folder_ready.pop_front() {
                let folder = b.loaded_folders.get(&item.workspace_root);
                let is_ready = folder.is_none_or(FolderState::is_ready_or_timed_out);
                let timed_out = folder.is_some_and(|f| !f.ready && f.is_ready_or_timed_out());
                if is_ready {
                    ready.push((item, timed_out));
                } else {
                    keep.push_back(item);
                }
            }
            b.pending_folder_ready = keep;
            ready
        };
        for (item, timed_out) in queued {
            let Some(c) = self.clients.get(&item.client_id) else {
                continue;
            };
            if timed_out
                && c.is_cli
                && item.msg.get("method").and_then(Value::as_str) == Some("textDocument/diagnostic")
                && let Some(req_id) = item.msg.get("id").cloned()
            {
                c.send(&json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {
                        "code": -32001,
                        "message": "rust-analyzer workspace analysis did not finish within ready timeout"
                    }
                }));
                continue;
            }
            self.send_request_or_notif(bid, item.client_id, item.msg);
        }
    }

    fn on_fire_retry(&mut self, bid: BackendId, token: u64) {
        let Some(mut req) = self
            .backends
            .get_mut(&bid)
            .and_then(|b| b.retrying.remove(&token))
        else {
            return;
        };
        req.waiters
            .retain(|w| !w.cancelled && self.clients.contains_key(&w.client_id));
        if req.waiters.is_empty() {
            return;
        }
        let Some(b) = self.backends.get_mut(&bid) else {
            return;
        };
        let backend_id = b.next_req_id;
        b.next_req_id += 1;
        req.generation = b.generation;
        let mut msg = req.original_msg.clone();
        msg["id"] = json!(backend_id);
        b.in_flight.insert(backend_id, req);
        b.send(&msg);
    }

    fn on_backend_exited(&mut self, bid: BackendId) {
        let Some(mut b) = self.backends.remove(&bid) else {
            return;
        };
        b.cancelled.store(true, Ordering::Release);
        if self.backend_by_key.get(&b.key) == Some(&bid) {
            self.backend_by_key.remove(&b.key);
        }
        // Reap any orphaned proc-macro-srv / cargo children in the crashed
        // backend's process group BEFORE `wait()` reaps `b.pid` (so the PGID
        // cannot be reused and shared `target_dir` locks release immediately).
        if let Some(mut child) = b.child.take() {
            let pgrp = b.pid as libc::pid_t;
            unsafe {
                libc::kill(-pgrp, libc::SIGKILL);
            }
            let _ = child.wait();
        }

        // Fail any in-flight or retrying requests cleanly so clients never hang.
        for req in b.in_flight.into_values().chain(b.retrying.into_values()) {
            for w in req.waiters {
                if !w.cancelled
                    && let Some(c) = self.clients.get(&w.client_id)
                {
                    c.send(&json!({
                        "jsonrpc": "2.0",
                        "id": w.client_req_id,
                        "error": { "code": -32603, "message": "rust-analyzer backend exited unexpectedly" }
                    }));
                }
            }
        }
        for q in b.pending_folder_ready {
            if let Some(req_id) = q.msg.get("id").cloned()
                && let Some(c) = self.clients.get(&q.client_id)
            {
                c.send(&json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": { "code": -32603, "message": "rust-analyzer backend exited unexpectedly" }
                }));
            }
        }
        for (cid, q) in b.pending_init {
            if let Some(req_id) = q.get("id").cloned()
                && let Some(c) = self.clients.get(&cid)
            {
                c.send(&json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": { "code": -32603, "message": "rust-analyzer backend exited unexpectedly" }
                }));
            }
        }
        // Mark attached clients dormant so their next request transparently spawns a new backend.
        for c in self.clients.values_mut() {
            if c.backend_id == Some(bid) {
                c.active = false;
                c.backend_id = None;
                if let Some(req_id) = c.waiting_init_req.take() {
                    c.send(&json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "error": { "code": -32603, "message": "rust-analyzer backend exited during initialize" }
                    }));
                }
            }
        }
        if self.clients.is_empty() && self.backends.is_empty() {
            self.empty_since.get_or_insert_with(Instant::now);
        }
    }

    fn stop_backend(
        &mut self,
        bid: BackendId,
        counted_as_eviction: bool,
    ) -> Option<thread::JoinHandle<()>> {
        let mut b = self.backends.remove(&bid)?;
        b.cancelled.store(true, Ordering::Release);
        if self.backend_by_key.get(&b.key) == Some(&bid) {
            self.backend_by_key.remove(&b.key);
        }
        if counted_as_eviction {
            self.backends_evicted += 1;
        } else {
            self.backends_hibernated += 1;
        }
        for c in self.clients.values_mut() {
            if c.backend_id == Some(bid) {
                c.active = false;
                c.backend_id = None;
            }
        }
        if b.child.is_some() {
            b.send(
                &json!({ "jsonrpc": "2.0", "id": b.next_req_id, "method": "shutdown", "params": null }),
            );
            b.send(&json!({ "jsonrpc": "2.0", "method": "exit", "params": null }));
        }
        drop(b.tx);
        let setup_handle = b.setup_handle.take();
        let Some(mut child) = b.child.take() else {
            return setup_handle;
        };
        let pid = b.pid;
        Some(thread::spawn(move || {
            let pgrp = pid as libc::pid_t;
            let deadline = Instant::now() + Duration::from_millis(1200);
            while Instant::now() < deadline {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            unsafe {
                libc::kill(-pgrp, libc::SIGTERM);
            }
            let term_deadline = Instant::now() + Duration::from_millis(300);
            while Instant::now() < term_deadline {
                if matches!(child.try_wait(), Ok(Some(_))) && unsafe { libc::kill(-pgrp, 0) } != 0 {
                    return;
                }
                thread::sleep(Duration::from_millis(25));
            }
            unsafe {
                libc::kill(-pgrp, libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            for _ in 0..20 {
                if unsafe { libc::kill(-pgrp, 0) } != 0 {
                    break;
                }
                thread::sleep(Duration::from_millis(25));
            }
        }))
    }

    fn shutdown_all_backends(&mut self) {
        let ids: Vec<BackendId> = self.backends.keys().copied().collect();
        let handles: Vec<_> = ids
            .into_iter()
            .filter_map(|bid| self.stop_backend(bid, false))
            .collect();
        for h in handles {
            let _ = h.join();
        }
    }

    fn refresh_rss(&mut self) {
        let pids: Vec<u32> = self
            .backends
            .values()
            .map(|b| b.pid)
            .filter(|&p| p != 0)
            .collect();
        let measured = measure_group_rss(&pids);
        for b in self.backends.values_mut() {
            if let Some(&rss) = measured.get(&b.pid) {
                b.rss_bytes = rss;
            }
        }
    }

    fn enforce_capacity_before_spawn(&mut self) {
        if self.cfg.max_backends == 0 || self.backends.len() < self.cfg.max_backends {
            return;
        }
        if let Some(victim) = self.pick_eviction_victim() {
            self.stop_backend(victim, true);
        }
    }

    fn pick_eviction_victim(&self) -> Option<BackendId> {
        // Never evict a backend that is still seeding/initializing or has
        // in-flight, retrying, or queued requests; prefer 0 active clients,
        // then oldest last_activity.
        self.backends
            .values()
            .filter(|b| {
                b.pid != 0
                    && b.ready
                    && b.in_flight.is_empty()
                    && b.retrying.is_empty()
                    && b.pending_folder_ready.is_empty()
                    && b.pending_init.is_empty()
                    && b.pending_added_folders.is_empty()
            })
            .min_by_key(|b| {
                let active = self
                    .clients
                    .values()
                    .filter(|c| c.active && c.backend_id == Some(b.id))
                    .count();
                (active > 0, b.last_activity)
            })
            .map(|b| b.id)
    }

    fn client_has_pending_work(&self, cid: ClientId) -> bool {
        let Some(bid) = self.clients.get(&cid).and_then(|c| c.backend_id) else {
            return false;
        };
        let Some(b) = self.backends.get(&bid) else {
            return false;
        };
        b.in_flight.values().any(|r| r.has_active_client(cid))
            || b.retrying.values().any(|r| r.has_active_client(cid))
            || b.pending_init.iter().any(|(c, _)| *c == cid)
            || b.pending_folder_ready.iter().any(|q| q.client_id == cid)
    }

    fn on_tick(&mut self) {
        let now = Instant::now();
        self.tick_count += 1;

        // 1. Flush any workspace-ready queues whose safety timeout has elapsed.
        let bids: Vec<BackendId> = self.backends.keys().copied().collect();
        for bid in bids {
            self.flush_ready_queue(bid);
        }

        // 2. Hibernate idle clients (keeping their sockets and open_docs alive!),
        //    never detaching a client with an active/queued request.
        if !self.cfg.idle_timeout.is_zero() {
            let idle_clients: Vec<ClientId> = self
                .clients
                .values()
                .filter(|c| {
                    c.active
                        && !self.client_has_pending_work(c.id)
                        && now.saturating_duration_since(c.last_activity) >= self.cfg.idle_timeout
                })
                .map(|c| c.id)
                .collect();
            for cid in idle_clients {
                self.detach_client_from_backend(cid);
            }
        }

        // 3. Remove unused sibling worktrees from multi-worktree backends and
        //    hibernate backends with zero active clients past idle grace.
        let mut to_stop = Vec::new();
        for (&bid, b) in &mut self.backends {
            let mut active_ws: HashSet<&Path> = HashSet::new();
            for c in self
                .clients
                .values()
                .filter(|c| c.active && c.backend_id == Some(bid))
            {
                if let Some(ws) = c.workspace_root.as_deref() {
                    active_ws.insert(ws);
                }
                for ef in c.extra_folders.keys() {
                    active_ws.insert(ef.as_path());
                }
            }

            if active_ws.is_empty() {
                if b.pid == 0 {
                    continue;
                }
                let grace = if self.clients.values().any(|c| c.backend_id == Some(bid)) {
                    if self.cfg.idle_timeout.is_zero() {
                        continue;
                    }
                    self.cfg.idle_timeout
                } else if b.cli_warmed {
                    if self.cfg.idle_timeout.is_zero() {
                        ORPHAN_BACKEND_GRACE
                    } else {
                        self.cfg.idle_timeout
                    }
                } else if self.cfg.idle_timeout.is_zero() {
                    ORPHAN_BACKEND_GRACE
                } else {
                    self.cfg.idle_timeout.min(ORPHAN_BACKEND_GRACE)
                };
                if b.in_flight.is_empty()
                    && b.retrying.is_empty()
                    && b.pending_init.is_empty()
                    && b.pending_folder_ready.is_empty()
                    && b.pending_added_folders.is_empty()
                    && now.saturating_duration_since(b.last_activity) >= grace
                {
                    to_stop.push(bid);
                }
                continue;
            }

            // If a backend has >1 loaded folders, unload idle folders that no active
            // client currently uses.
            if b.ready && b.loaded_folders.len() > 1 {
                let folder_grace = if self.cfg.idle_timeout.is_zero() {
                    ORPHAN_BACKEND_GRACE
                } else if b.cli_warmed {
                    self.cfg.idle_timeout
                } else {
                    self.cfg.idle_timeout.min(ORPHAN_BACKEND_GRACE)
                };
                let stale: Vec<(PathBuf, String)> = b
                    .loaded_folders
                    .iter()
                    .filter(|(p, f)| {
                        !active_ws.contains(p.as_path())
                            && now.saturating_duration_since(f.last_active) >= folder_grace
                    })
                    .map(|(p, f)| (p.clone(), f.uri.clone()))
                    .collect();
                if !stale.is_empty() {
                    let mut removed = Vec::new();
                    for (path, uri) in stale {
                        b.loaded_folders.remove(&path);
                        let name = path
                            .file_name()
                            .and_then(OsStr::to_str)
                            .unwrap_or("workspace");
                        removed.push(json!({ "uri": uri, "name": name }));
                    }
                    b.send(&json!({
                        "jsonrpc": "2.0",
                        "method": "workspace/didChangeWorkspaceFolders",
                        "params": { "event": { "added": [], "removed": removed } }
                    }));
                }
            }
        }
        for bid in to_stop {
            self.stop_backend(bid, false);
        }

        // 4. Periodically refresh RSS and enforce memory & max-backend budgets via LRU eviction.
        if self.tick_count.is_multiple_of(RSS_REFRESH_TICKS)
            || (self.cfg.max_backends > 0 && self.backends.len() > self.cfg.max_backends)
        {
            self.refresh_rss();
            if self.cfg.memory_budget > 0 {
                while self.backends.len() > 1 {
                    let total: u64 = self.backends.values().map(|b| b.rss_bytes).sum();
                    if total <= self.cfg.memory_budget {
                        break;
                    }
                    let Some(victim) = self.pick_eviction_victim() else {
                        break;
                    };
                    self.stop_backend(victim, true);
                }
            }
        }
        if self.cfg.max_backends > 0 {
            while self.backends.len() > self.cfg.max_backends {
                let Some(victim) = self.pick_eviction_victim() else {
                    break;
                };
                self.stop_backend(victim, true);
            }
        }

        // 5. Auto-exit when no clients and no backends remain past empty grace.
        if self.clients.is_empty() && self.backends.is_empty() {
            let since = *self.empty_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= EMPTY_DAEMON_GRACE {
                self.shutdown = true;
            }
        } else {
            self.empty_since = None;
        }
    }
}

fn run_daemon() -> Result<i32> {
    let cache = cache_dir()?;
    fs::create_dir_all(cache.join("rust-analyzer"))?;
    let addr = daemon_addr(&cache)?;
    let listener = match UnixListener::bind_addr(&addr) {
        Ok(l) => l,
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => return Ok(0),
        Err(e) => return Err(e).context("binding rust-analyzer daemon socket"),
    };

    let (tx, rx): (Sender<DaemonEvent>, Receiver<DaemonEvent>) = mpsc::channel();
    let cfg = DaemonConfig::from_env(cache);

    // Accept connections and verify peer UID (`SO_PEERCRED`) before dispatching.
    let accept_tx = tx.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if !wire::same_user(&stream) {
                continue;
            }
            let ev_tx = accept_tx.clone();
            thread::spawn(move || {
                let mut stream = stream;
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let Ok(bytes) = wire::receive(&mut stream) else {
                    return;
                };
                let _ = stream.set_read_timeout(None);
                let Ok(hs) = serde_json::from_slice::<Handshake>(&bytes) else {
                    return;
                };
                let event = match hs {
                    Handshake::Status => DaemonEvent::StatusRequest(stream),
                    Handshake::Stop => DaemonEvent::StopRequest(stream),
                    Handshake::Lsp {
                        cwd,
                        overrides,
                        cli,
                        ..
                    } => DaemonEvent::NewLspClient {
                        cwd,
                        overrides,
                        cli,
                        stream,
                    },
                };
                let _ = ev_tx.send(event);
            });
        }
    });

    // Periodic maintenance ticker (RSS checks, idle hibernation, LRU eviction).
    let tick_tx = tx.clone();
    thread::spawn(move || {
        loop {
            thread::sleep(TICK_INTERVAL);
            if tick_tx.send(DaemonEvent::Tick).is_err() {
                break;
            }
        }
    });

    let sig_tx = tx.clone();
    let _ = ctrlc::set_handler(move || {
        let _ = sig_tx.send(DaemonEvent::SignalStop);
    });

    let mut daemon = Daemon::new(cfg, tx);
    for event in &rx {
        daemon.handle(event);
        if daemon.shutdown {
            break;
        }
    }
    // Join all backend shutdown/setup threads first, then drain any queued
    // `BackendSpawned` events so `ManagedChild::drop` reaps every child before
    // acknowledging `jato rust-analyzer stop`.
    daemon.shutdown_all_backends();
    while let Ok(ev) = rx.try_recv() {
        drop(ev);
    }
    if let Some(mut stream) = daemon.stop_reply.take() {
        let _ = wire::send(&mut stream, br#"{"ok":true}"#);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_lsp_vs_cli_invocations() {
        let os = |s: &[&str]| s.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(is_lsp_invocation(&os(&[])));
        assert!(is_lsp_invocation(&os(&["-v", "--no-log-buffering"])));
        assert!(is_lsp_invocation(&os(&[
            "--log-file",
            "/tmp/ra.log",
            "-vv"
        ])));
        assert!(is_lsp_invocation(&os(&["--log-file=/tmp/ra.log"])));

        assert!(!is_lsp_invocation(&os(&["--version"])));
        assert!(!is_lsp_invocation(&os(&["-V"])));
        assert!(!is_lsp_invocation(&os(&["--help"])));
        assert!(!is_lsp_invocation(&os(&["--print-config-schema"])));
        assert!(!is_lsp_invocation(&os(&["analysis-stats", "."])));
        assert!(!is_lsp_invocation(&os(&["rustc", "--crate-name", "foo"])));
    }

    #[test]
    fn round_trips_file_uris_and_matches_workspace_boundaries() {
        let p = Path::new("/home/ben/my repo/src/main#1.rs");
        let uri = path_to_uri(p);
        assert_eq!(uri, "file:///home/ben/my%20repo/src/main%231.rs");
        assert_eq!(uri_to_path(&uri).as_deref(), Some(p));

        assert!(uri_in_root(
            "file:///home/ben/src/olo/crates/app/src/lib.rs",
            "file:///home/ben/src/olo"
        ));
        assert!(!uri_in_root(
            "file:///home/ben/src/olo-other/src/lib.rs",
            "file:///home/ben/src/olo"
        ));
    }

    #[test]
    fn applies_lsp_utf16_incremental_and_full_text_changes() {
        let mut text = String::from("fn main() {\n    let s = \"🦀\";\n}\n");
        // Replace "🦀" (UTF-16 length 2 at col 13..15) with "ok"
        apply_content_changes(
            &mut text,
            &[json!({
                "range": {
                    "start": { "line": 1, "character": 13 },
                    "end": { "line": 1, "character": 15 }
                },
                "text": "ok"
            })],
        );
        assert_eq!(text, "fn main() {\n    let s = \"ok\";\n}\n");

        // Full replacement
        apply_content_changes(&mut text, &[json!({ "text": "fn x() {}\n" })]);
        assert_eq!(text, "fn x() {}\n");
    }

    #[test]
    fn filters_sibling_worktree_uris_while_preserving_own_and_external() {
        let others = vec!["file:///wt/b".to_owned()];
        let mut syms = json!([
            { "name": "Foo", "location": { "uri": "file:///wt/a/src/lib.rs" } },
            { "name": "Foo", "location": { "uri": "file:///wt/b/src/lib.rs" } },
            { "name": "Bar", "location": { "uri": "file:///home/ben/.cargo/registry/src/x/lib.rs" } }
        ]);
        filter_response_for_workspace("workspace/symbol", &mut syms, &others);
        assert_eq!(syms.as_array().unwrap().len(), 2);
        assert_eq!(syms[0]["location"]["uri"], "file:///wt/a/src/lib.rs");
        assert_eq!(
            syms[1]["location"]["uri"],
            "file:///home/ben/.cargo/registry/src/x/lib.rs"
        );

        let mut refs = json!([
            { "uri": "file:///wt/a/src/lib.rs" },
            { "uri": "file:///wt/b/src/lib.rs" },
            { "uri": "file:///wt/b-extra/src/lib.rs" }
        ]);
        filter_response_for_workspace("textDocument/references", &mut refs, &others);
        assert_eq!(refs.as_array().unwrap().len(), 2);
        assert_eq!(refs[1]["uri"], "file:///wt/b-extra/src/lib.rs");
    }

    #[test]
    fn finds_shared_git_common_dir_across_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("repo");
        let wt = tmp.path().join("wt1");
        fs::create_dir_all(main.join(".git/worktrees/wt1")).unwrap();
        fs::create_dir_all(&wt).unwrap();
        fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", main.join(".git/worktrees/wt1").display()),
        )
        .unwrap();
        fs::write(main.join(".git/worktrees/wt1/commondir"), "../..\n").unwrap();

        assert_eq!(git_common_dir(&main), git_common_dir(&wt));
        assert!(git_common_dir(&wt).is_some());
    }

    #[test]
    fn merges_and_queries_rust_analyzer_config_sections() {
        let mut base = json!({
            "numThreads": 4,
            "cachePriming": { "enable": false, "numThreads": 4 },
            "cargo": { "targetDir": "/tmp/shared" }
        });
        let client = json!({
            "cargo": { "features": "all" },
            "checkOnSave": false
        });
        deep_merge(&mut base, &client);
        assert_eq!(
            config_section(&base, Some("rust-analyzer.cargo.targetDir")),
            json!("/tmp/shared")
        );
        assert_eq!(
            config_section(&base, Some("rust-analyzer.cargo.features")),
            json!("all")
        );
        assert_eq!(
            config_section(&base, Some("rust-analyzer.checkOnSave")),
            json!(false)
        );
        assert_eq!(config_section(&base, Some("rust-analyzer")), base);
    }

    #[test]
    fn cache_backed_settings_wire_wrapper_and_check_override_respecting_client_opts() {
        let cfg = DaemonConfig {
            cache_dir: PathBuf::from("/tmp/jato-cache"),
            target_root: PathBuf::from("/tmp/jato-cache-ra-targets"),
            memory_budget: 4 << 30,
            max_backends: 8,
            idle_timeout: Duration::from_secs(600),
            share_worktrees: true,
            lazy: true,
            threads: 4,
            prime: false,
            proc_macros: 1,
            lru_capacity: 128,
            shared_target_dir: None,
            check_on_save: None,
            cache: true,
            seed: true,
            check_via_jato: true,
        };
        let group = Path::new("/repo/.git");
        let rustc_bin = Path::new("/usr/bin/rustc");
        let ws = Path::new("/repo");
        let exe = Path::new("/home/ben/.cargo/bin/jato");

        // 1. Default config wires RUSTC_WRAPPER, JATO_CACHE_DIR, check.overrideCommand,
        //    and an isolated sibling `-check` target dir for checkOnSave.
        let (settings, eff) = cfg.merged_ra_settings_with_exe(
            group,
            rustc_bin,
            ws,
            &json!({}),
            &ClientEnvOverrides::default(),
            Some(exe),
        );
        assert!(eff.cache_backed);
        assert!(eff.seed);
        assert!(eff.check_via_jato);
        let tdir = eff.target_dir.as_ref().unwrap();
        let check_dir = eff.check_target_dir.as_ref().unwrap();
        assert_ne!(tdir, check_dir);
        assert!(check_dir.to_string_lossy().ends_with("-check"));
        assert_eq!(
            settings.pointer("/cargo/extraEnv/RUSTC_WRAPPER"),
            Some(&json!("/home/ben/.cargo/bin/jato"))
        );
        assert_eq!(
            settings.pointer("/cargo/extraEnv/JATO_CACHE_DIR"),
            Some(&json!("/tmp/jato-cache"))
        );
        assert_eq!(
            settings.pointer("/check/overrideCommand"),
            Some(&json!([
                "/home/ben/.cargo/bin/jato",
                "check",
                "--workspace",
                "--all-targets",
                "--message-format=json"
            ]))
        );
        assert_eq!(
            settings.pointer("/check/extraEnv/CARGO_TARGET_DIR"),
            Some(&json!(check_dir.to_string_lossy()))
        );

        // 2. Client customizing check.command (clippy) or cargo.features / cargo.target
        //    keeps client settings instead of forcing overrideCommand.
        let (settings, eff) = cfg.merged_ra_settings_with_exe(
            group,
            rustc_bin,
            ws,
            &json!({ "check": { "command": "clippy" } }),
            &ClientEnvOverrides::default(),
            Some(exe),
        );
        assert!(eff.cache_backed);
        assert!(!eff.check_via_jato);
        assert!(settings.pointer("/check/overrideCommand").is_none());
        assert_eq!(settings.pointer("/check/command"), Some(&json!("clippy")));

        let (settings, eff) = cfg.merged_ra_settings_with_exe(
            group,
            rustc_bin,
            ws,
            &json!({ "cargo": { "features": "all" } }),
            &ClientEnvOverrides::default(),
            Some(exe),
        );
        assert!(eff.cache_backed);
        assert!(!eff.seed);
        assert!(!eff.check_via_jato);
        assert!(settings.pointer("/check/overrideCommand").is_none());

        // 3. Client disabling checkOnSave (bool or legacy object) skips overrideCommand.
        for opt in [
            json!({ "checkOnSave": false }),
            json!({ "checkOnSave": { "enable": false } }),
        ] {
            let (settings, eff) = cfg.merged_ra_settings_with_exe(
                group,
                rustc_bin,
                ws,
                &opt,
                &ClientEnvOverrides::default(),
                Some(exe),
            );
            assert!(!eff.check_via_jato);
            assert!(settings.pointer("/check/overrideCommand").is_none());
        }

        // 4. JATO_RA_CACHE=0 or client overriding RUSTC_WRAPPER disables wrapper,
        //    seeding, and check override cleanly.
        let ov_off = ClientEnvOverrides {
            cache: Some(false),
            ..ClientEnvOverrides::default()
        };
        let (settings, eff) =
            cfg.merged_ra_settings_with_exe(group, rustc_bin, ws, &json!({}), &ov_off, Some(exe));
        assert!(!eff.cache_backed);
        assert!(!eff.seed);
        assert!(!eff.check_via_jato);
        assert!(settings.pointer("/cargo/extraEnv/RUSTC_WRAPPER").is_none());
        assert!(settings.pointer("/check/overrideCommand").is_none());

        let (settings, eff) = cfg.merged_ra_settings_with_exe(
            group,
            rustc_bin,
            ws,
            &json!({ "cargo": { "extraEnv": { "RUSTC_WRAPPER": "sccache" } } }),
            &ClientEnvOverrides::default(),
            Some(exe),
        );
        assert!(!eff.cache_backed);
        assert!(!eff.seed);
        assert!(!eff.check_via_jato);
        assert!(settings.pointer("/check/overrideCommand").is_none());

        // 5. Client custom cargo.targetDir (relative resolved against ws, and boolean `true`).
        let (settings, eff) = cfg.merged_ra_settings_with_exe(
            group,
            rustc_bin,
            ws,
            &json!({ "cargo": { "targetDir": "target/ra-custom" } }),
            &ClientEnvOverrides::default(),
            Some(exe),
        );
        assert_eq!(
            eff.target_dir.as_deref(),
            Some(Path::new("/repo/target/ra-custom"))
        );
        assert_eq!(
            settings.pointer("/check/extraEnv/CARGO_TARGET_DIR"),
            Some(&json!("/repo/target/ra-custom-check"))
        );

        let (settings, eff) = cfg.merged_ra_settings_with_exe(
            group,
            rustc_bin,
            ws,
            &json!({ "cargo": { "targetDir": true } }),
            &ClientEnvOverrides::default(),
            Some(exe),
        );
        assert!(eff.target_dir.is_none());
        assert!(!eff.seed);
        assert!(!eff.check_via_jato);
        assert!(settings.pointer("/check/overrideCommand").is_none());

        // 6. Client passing `extraEnv: null` must not break JATO_RA_CACHE=1 injection.
        let ov_force = ClientEnvOverrides {
            cache: Some(true),
            ..ClientEnvOverrides::default()
        };
        let (settings, eff) = cfg.merged_ra_settings_with_exe(
            group,
            rustc_bin,
            ws,
            &json!({ "cargo": { "extraEnv": null } }),
            &ov_force,
            Some(exe),
        );
        assert!(eff.cache_backed);
        assert_eq!(
            settings.pointer("/cargo/extraEnv/RUSTC_WRAPPER"),
            Some(&json!("/home/ben/.cargo/bin/jato"))
        );
    }

    struct Rng(u64);
    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed.max(1))
        }
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn usize(&mut self, bound: usize) -> usize {
            if bound == 0 {
                0
            } else {
                (self.next() as usize) % bound
            }
        }
    }

    /// Reader that delivers at most `step` bytes per `read` to torture chunk boundaries.
    struct ChunkedReader<'a> {
        data: &'a [u8],
        pos: usize,
        step: usize,
    }
    impl Read for ChunkedReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let rem = self.data.len().saturating_sub(self.pos);
            let n = rem.min(self.step).min(buf.len());
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    #[test]
    fn fuzz_lsp_framing_parser() {
        let mut rng = Rng::new(0xC0FF_EE12_3456_789A);

        // 1. Round-trip 500 randomized valid JSON-RPC objects across tiny chunk sizes.
        for i in 0..500u64 {
            let msg = json!({
                "jsonrpc": "2.0",
                "id": if i % 3 == 0 { json!(format!("str-{i}")) } else { json!(i) },
                "method": "textDocument/hover",
                "params": {
                    "text": format!("🦀 unicode {} {}", "x".repeat(rng.usize(256)), i),
                    "n": rng.next()
                }
            });
            let mut wire = encode_lsp_message(&msg);
            if i % 2 == 0 {
                // Inject extra benign header before Content-Length.
                let mut prefixed =
                    b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n".to_vec();
                prefixed.extend_from_slice(&wire);
                wire = prefixed;
            }
            let step = [1, 2, 3, 7, 19, 64][rng.usize(6)];
            let mut r = BufReader::with_capacity(
                step.max(1),
                ChunkedReader {
                    data: &wire,
                    pos: 0,
                    step,
                },
            );
            let parsed = read_lsp_message(&mut r).unwrap().unwrap();
            assert_eq!(parsed, msg);
            assert!(read_lsp_message(&mut r).unwrap().is_none());
        }

        // 2. Adversarial header & payload rejection (must never panic or OOM).
        let bad_cases: Vec<Vec<u8>> = vec![
            // Header flood (> MAX_LSP_HEADERS)
            format!(
                "{}Content-Length: 2\r\n\r\n{{}}",
                "X-Pad: a\r\n".repeat(MAX_LSP_HEADERS + 2)
            )
            .into_bytes(),
            // Overlong single header line (> MAX_LSP_HEADER_LINE)
            format!("X-Huge: {}\r\n\r\n", "A".repeat(MAX_LSP_HEADER_LINE + 64)).into_bytes(),
            // Huge Content-Length exceeding MAX_LSP_MESSAGE
            format!("Content-Length: {}\r\n\r\n{{}}", MAX_LSP_MESSAGE + 1).into_bytes(),
            // Signed / non-digit Content-Length
            b"Content-Length: +2\r\n\r\n{}".to_vec(),
            b"Content-Length: -1\r\n\r\n{}".to_vec(),
            b"Content-Length: abc\r\n\r\n{}".to_vec(),
            // Missing Content-Length
            b"Content-Type: text/plain\r\n\r\n{}".to_vec(),
            // Truncated header
            b"Content-Length: 2\r\n".to_vec(),
            // Truncated body
            b"Content-Length: 10\r\n\r\n{}".to_vec(),
            // Invalid UTF-8 in header
            vec![b'X', b':', 0xFF, b'\r', b'\n', b'\r', b'\n'],
            // Valid JSON but NOT a JSON-RPC object
            b"Content-Length: 4\r\n\r\nnull".to_vec(),
            b"Content-Length: 3\r\n\r\n123".to_vec(),
            b"Content-Length: 7\r\n\r\n[1,2,3]".to_vec(),
            b"Content-Length: 4\r\n\r\n\"hi\"".to_vec(),
        ];
        for case in bad_cases {
            let mut r = BufReader::new(case.as_slice());
            assert!(
                read_lsp_message(&mut r).is_err(),
                "expected error for {:?}",
                String::from_utf8_lossy(&case[..case.len().min(80)])
            );
        }

        // 3. Pure random byte fuzzing (1,000 arbitrary buffers must never panic).
        for _ in 0..1000 {
            let len = rng.usize(256);
            let mut raw = vec![0u8; len];
            for b in &mut raw {
                *b = (rng.next() & 0xFF) as u8;
            }
            let mut r = BufReader::new(raw.as_slice());
            let _ = read_lsp_message(&mut r);
        }
    }

    #[test]
    fn fuzz_lsp_utf16_content_changes_against_oracle() {
        // Independent UTF-16 oracle: splits on `\n`, strips optional trailing `\r`
        // for column indexing, replaces in UTF-16 code units, re-encodes UTF-8.
        fn oracle_pos(text: &str, target_line: usize, target_u16: usize) -> usize {
            let lines: Vec<&str> = text.split_inclusive('\n').collect();
            if target_line >= lines.len() {
                return text.len();
            }
            let prefix: usize = lines[..target_line].iter().map(|s| s.len()).sum();
            let raw = lines[target_line];
            let body = raw
                .strip_suffix('\n')
                .unwrap_or(raw)
                .strip_suffix('\r')
                .unwrap_or(raw.strip_suffix('\n').unwrap_or(raw));
            let mut u16_acc = 0usize;
            for (bidx, ch) in body.char_indices() {
                if u16_acc >= target_u16 {
                    return prefix + bidx;
                }
                u16_acc += ch.len_utf16();
            }
            prefix + body.len()
        }

        let atoms = [
            "a",
            "fn ",
            "let x = 1;",
            "é",
            "λ",
            "中",
            "—",
            "🦀",
            "𝄞",
            "👨‍👩‍👧",
            "\n",
            "\r\n",
            "",
        ];
        let mut rng = Rng::new(0xBAD5_EED0_CAFE_BABE);

        for _ in 0..600 {
            let mut doc = String::new();
            let n_atoms = rng.usize(18);
            for _ in 0..n_atoms {
                doc.push_str(atoms[rng.usize(atoms.len())]);
            }
            let mut oracle = doc.clone();

            let n_edits = 1 + rng.usize(10);
            for _ in 0..n_edits {
                let mode = rng.usize(10);
                let mut repl = String::new();
                for _ in 0..rng.usize(4) {
                    repl.push_str(atoms[rng.usize(atoms.len())]);
                }
                if mode == 0 {
                    // Full document replacement.
                    let change = vec![json!({ "text": repl })];
                    apply_content_changes(&mut doc, &change);
                    oracle = repl;
                } else {
                    let line_count = doc.split_inclusive('\n').count() + 2;
                    let sl = rng.usize(line_count);
                    let el = rng.usize(line_count);
                    let sc = rng.usize(24);
                    let ec = rng.usize(24);
                    let change = vec![json!({
                        "range": {
                            "start": { "line": sl, "character": sc },
                            "end": { "line": el, "character": ec }
                        },
                        "text": repl
                    })];
                    apply_content_changes(&mut doc, &change);
                    let s = oracle_pos(&oracle, sl, sc);
                    let e = oracle_pos(&oracle, el, ec).max(s);
                    oracle.replace_range(s..e, &repl);
                }
                assert_eq!(doc, oracle);
            }
        }
    }

    #[test]
    fn fuzz_uri_round_trip_and_workspace_filtering() {
        let segs = [
            "home",
            "ben",
            "my repo",
            "wt#1",
            "a%20b",
            "c?d=e",
            "日本語",
            "🦀rust",
            "foo+bar",
            "sub-dir_2.0~x",
        ];
        let mut rng = Rng::new(0x1337_BEEF_9876_5432);

        for _ in 0..500 {
            let depth = 1 + rng.usize(5);
            let mut p = PathBuf::from("/");
            for _ in 0..depth {
                p.push(segs[rng.usize(segs.len())]);
            }
            let uri = path_to_uri(&p);
            assert_eq!(uri_to_path(&uri).as_deref(), Some(p.as_path()), "uri={uri}");
            assert!(uri_in_root(&uri, &uri));

            let child = p.join("src/lib.rs");
            let child_uri = path_to_uri(&child);
            assert!(uri_in_root(&child_uri, &uri));
            assert!(uri_in_root(&child_uri, &format!("{uri}/")));
            // Lowercase percent-encoding must still match uppercase root URI.
            let lower_child = child_uri.to_ascii_lowercase();
            if !child
                .to_string_lossy()
                .chars()
                .any(|c| c.is_ascii_uppercase())
            {
                assert!(uri_in_root(&lower_child, &uri), "{lower_child} vs {uri}");
            }
            // localhost authority form must also match.
            let localhost_child = child_uri.replacen("file:///", "file://localhost/", 1);
            assert!(uri_in_root(&localhost_child, &uri));

            // Sibling prefix trap must NEVER match.
            let mut sib_os = p.into_os_string();
            sib_os.push("-other");
            let sib_uri = path_to_uri(&PathBuf::from(sib_os).join("src/lib.rs"));
            assert!(!uri_in_root(&sib_uri, &uri), "{sib_uri} vs {uri}");
        }

        // Malformed / edge URIs must never panic.
        for bad in [
            "",
            "http://example.com/a",
            "file://relative",
            "file:///a%2",
            "file:///a%ZZb",
            "file:///a%🦀",
            "file:///",
            "file://localhost/tmp/x",
        ] {
            let _ = uri_to_path(bad);
            let _ = uri_in_root(bad, "file:///tmp");
            let _ = uri_in_root("file:///tmp/a", bad);
        }

        // Filter stress test across all supported methods + arbitrary JSON shapes.
        let others = vec!["file:///wt/sib%201".to_owned(), "file:///wt/b".to_owned()];
        let mut rename = json!({
            "changes": {
                "file:///wt/a/src/lib.rs": [],
                "file:///wt/sib%201/src/lib.rs": [],
                "file:///wt/b/src/lib.rs": [],
                "file:///wt/b2/src/lib.rs": []
            },
            "documentChanges": [
                { "textDocument": { "uri": "file:///wt/a/src/lib.rs" }, "edits": [] },
                { "textDocument": { "uri": "file:///wt/b/src/lib.rs" }, "edits": [] },
                { "kind": "rename", "oldUri": "file:///wt/sib%201/old.rs", "newUri": "file:///wt/sib%201/new.rs" }
            ]
        });
        filter_response_for_workspace("textDocument/rename", &mut rename, &others);
        let ch = rename["changes"].as_object().unwrap();
        assert_eq!(ch.len(), 2);
        assert!(ch.contains_key("file:///wt/a/src/lib.rs"));
        assert!(ch.contains_key("file:///wt/b2/src/lib.rs"));
        assert_eq!(rename["documentChanges"].as_array().unwrap().len(), 1);

        for method in [
            "workspace/symbol",
            "textDocument/references",
            "textDocument/definition",
            "callHierarchy/incomingCalls",
            "callHierarchy/outgoingCalls",
            "typeHierarchy/subtypes",
            "textDocument/rename",
        ] {
            for shape in [Value::Null, json!(42), json!("x"), json!([]), json!({})] {
                let mut v = shape;
                filter_response_for_workspace(method, &mut v, &others);
            }
        }
    }

    #[test]
    fn shim_help_flags_succeed_without_touching_filesystem() {
        for args in [
            vec!["--help"],
            vec!["-h"],
            vec!["help"],
            vec!["install", "--help"],
            vec!["uninstall", "-h"],
            vec!["status", "--help"],
        ] {
            let os: Vec<OsString> = args.into_iter().map(OsString::from).collect();
            assert_eq!(cmd_shim(&os).unwrap(), 0, "args: {os:?}");
        }
        let bad = vec![OsString::from("bogus")];
        assert!(cmd_shim(&bad).is_err());
    }
}
