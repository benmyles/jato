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
use std::collections::{HashMap, HashSet, VecDeque};
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
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

pub const HELP: &str = "\
Usage
  jato rust-analyzer [LSP_FLAGS...]           Run as an LSP proxy sharing backends
  jato rust-analyzer status [--json]          Show daemon, shared backends, and RSS
  jato rust-analyzer stop                     Stop the shared daemon and all backends
  jato rust-analyzer shim install [--dir DIR] Install a transparent rust-analyzer shim
  jato rust-analyzer shim uninstall [--dir DIR]
                                              Remove the shim and restore any rustup link
  jato rust-analyzer shim status              Show active rust-analyzer shim status

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
  JATO_RA_LRU             Syntax tree LRU capacity per backend (default: 64)
  JATO_RA_TARGET_DIR      1 (default) shares per-repo cargo.targetDir; 0 disables; or custom path
  JATO_RA_CHECK_ON_SAVE   0 or 1 to override rust-analyzer checkOnSave
  JATO_RUST_ANALYZER      Explicit path to the real rust-analyzer binary
  JATO_RA_PASSTHROUGH=1   Bypass multiplexing and exec the real rust-analyzer directly
";

const TICK_INTERVAL: Duration = Duration::from_secs(1);
const RSS_REFRESH_TICKS: u64 = 5;
const RETRY_DELAY: Duration = Duration::from_millis(300);
const MAX_CONTENT_MODIFIED_RETRIES: u8 = 12;
const WORKSPACE_READY_WAIT: Duration = Duration::from_secs(25);
const ORPHAN_BACKEND_GRACE: Duration = Duration::from_secs(60);
const EMPTY_DAEMON_GRACE: Duration = Duration::from_secs(30);
const MAX_LSP_MESSAGE: usize = 64 << 20;

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
    let Ok(own) = env::current_exe().and_then(fs::canonicalize) else {
        return false;
    };
    fs::canonicalize(path).ok().is_some_and(|p| p == own)
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
    if let Some(tc) = toolchain {
        cmd.arg("--toolchain").arg(tc);
        cmd.env_remove("RUSTUP_TOOLCHAIN");
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
        if let Some(found) = rustup_which(&rustup, cwd, None, "rust-analyzer") {
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

fn resolve_rustc(cwd: &Path) -> PathBuf {
    if let Some(rustup) = find_rustup()
        && let Some(found) = rustup_which(&rustup, cwd, None, "rustc")
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

fn cmd_shim(args: &[OsString]) -> Result<i32> {
    let sub = args.first().and_then(|a| a.to_str());
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
    workspaces: Vec<PathBuf>,
}

fn connect_existing(cache: &Path) -> Option<UnixStream> {
    let addr = daemon_addr(cache).ok()?;
    UnixStream::connect_addr(&addr).ok()
}

fn connect_or_start_daemon(cache: &Path) -> Result<UnixStream> {
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
    lock_file.lock().context("locking rust-analyzer daemon")?;
    if let Some(stream) = connect_existing(cache) {
        return Ok(stream);
    }

    let exe = env::current_exe()?;
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

    let deadline = Instant::now() + Duration::from_secs(10);
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
        "Sharing    {} shared attaches, {} hibernated, {} budget-evicted (idle {} s)",
        status.backends_reused,
        status.backends_hibernated,
        status.backends_evicted,
        status.idle_secs
    );
    for b in &status.backends {
        println!(
            "  #{} pid={} rss={} clients={} docs={} {} group={}",
            b.id,
            b.pid,
            mib(b.rss_bytes),
            b.active_clients,
            b.open_docs,
            if b.quiescent { "ready" } else { "loading" },
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
    let mut stream = connect_or_start_daemon(&cache)?;
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let handshake = Handshake::Lsp {
        cwd,
        pid: std::process::id(),
        overrides: ClientEnvOverrides::from_env(),
    };
    wire::send(&mut stream, &serde_json::to_vec(&handshake)?)?;

    let mut reader = stream.try_clone()?;
    let mut writer = stream;

    let stdin_to_sock = thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = [0u8; 16 * 1024];
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
    let mut buf = [0u8; 16 * 1024];
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
    let _ = stdin_to_sock.join();
    Ok(0)
}

// ---- LSP framing, URI helpers, git common dir, and document sync -----------

fn read_lsp_message<R: BufRead>(reader: &mut R) -> io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();
    let mut saw_header = false;
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
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
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = v.trim().parse::<usize>().ok();
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
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
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
    let root = root_uri.trim_end_matches('/');
    uri == root
        || (uri.starts_with(root) && uri.as_bytes().get(root.len()).is_some_and(|&b| b == b'/'))
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
            let line_str = chunk.trim_end_matches(['\r', '\n']);
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
    let mut h = blake3::Hasher::new_derive_key("jato ra caps v1");
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
        .spawn()
        .with_context(|| format!("probing capabilities from {}", ra_bin.display()))?;

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
    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_dir_all(&empty);

    let result = result.context("rust-analyzer did not return InitializeResult during probe")?;
    let tmp = cache_path.with_extension("tmp");
    if fs::write(&tmp, serde_json::to_vec(&result)?).is_ok() {
        let _ = fs::rename(&tmp, &cache_path);
    }
    Ok(result)
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
                .unwrap_or(64),
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
        }
    }

    fn backend_target_dir(&self, key: &BackendKey, ov: &ClientEnvOverrides) -> Option<PathBuf> {
        let setting = ov
            .target_dir
            .as_deref()
            .or(self.shared_target_dir.as_deref());
        match setting.map(str::trim) {
            Some("0" | "false" | "off") => None,
            Some("1" | "true" | "on") | None => {
                let mut h = blake3::Hasher::new_derive_key("jato ra target v1");
                h.update(key.group.as_os_str().as_bytes());
                h.update(&[0]);
                h.update(key.rustc_bin.as_os_str().as_bytes());
                Some(self.target_root.join(&h.finalize().to_hex()[..16]))
            }
            Some(custom) => Some(PathBuf::from(custom)),
        }
    }

    fn merged_ra_settings(
        &self,
        key: &BackendKey,
        client_opts: &Value,
        ov: &ClientEnvOverrides,
    ) -> Value {
        let threads = ov.threads.unwrap_or(self.threads);
        let prime = ov.prime.unwrap_or(self.prime);
        let proc_macros = ov.proc_macros.unwrap_or(self.proc_macros);
        let lru = ov.lru.unwrap_or(self.lru_capacity);
        let check_on_save = ov.check_on_save.or(self.check_on_save);
        let target_dir = self.backend_target_dir(key, ov);

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
        if let Some(dir) = target_dir {
            let _ = fs::create_dir_all(&dir);
            base["cargo"] = json!({ "targetDir": dir.to_string_lossy() });
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
        base
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
}

#[derive(Clone, Debug)]
struct ClientDoc {
    language_id: String,
    text: String,
}

struct ClientState {
    id: ClientId,
    cwd: PathBuf,
    overrides: ClientEnvOverrides,
    tx: Sender<Vec<u8>>,
    workspace_root: Option<PathBuf>,
    workspace_uri: Option<String>,
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
        let _ = self.tx.send(encode_lsp_message(msg));
    }
}

struct BackendDoc {
    holders: HashSet<ClientId>,
    version: i64,
    text: String,
}

struct InFlightRequest {
    client_id: ClientId,
    client_req_id: Value,
    method: String,
    original_msg: Value,
    retries_left: u8,
    cancelled: bool,
}

struct QueuedReadyMsg {
    client_id: ClientId,
    workspace_root: PathBuf,
    msg: Value,
}

struct BackendState {
    id: BackendId,
    key: BackendKey,
    child: Child,
    pid: u32,
    tx: Sender<Vec<u8>>,
    target_dir: Option<PathBuf>,
    ra_settings: Value,
    init_result: Option<Value>,
    ready: bool,
    quiescent: bool,
    loaded_folders: HashMap<PathBuf, FolderState>,
    open_docs: HashMap<String, BackendDoc>,
    next_req_id: u64,
    in_flight: HashMap<u64, InFlightRequest>,
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
    saw_busy: bool,
    ready: bool,
    last_active: Instant,
}

impl BackendState {
    fn send(&self, msg: &Value) {
        let _ = self.tx.send(encode_lsp_message(msg));
    }

    fn other_root_uris(&self, client_root: Option<&Path>) -> Vec<String> {
        self.loaded_folders
            .iter()
            .filter(|(p, _)| client_root.is_none_or(|c| c != p.as_path()))
            .map(|(_, f)| f.uri.clone())
            .collect()
    }
}

enum DaemonEvent {
    NewLspClient {
        cwd: PathBuf,
        overrides: ClientEnvOverrides,
        stream: UnixStream,
    },
    StatusRequest(UnixStream),
    StopRequest(UnixStream),
    ClientMessage(ClientId, Value),
    ClientDisconnected(ClientId),
    CapsProbed {
        client_id: ClientId,
        ra_bin: PathBuf,
        result: Result<Value, String>,
    },
    BackendMessage(BackendId, Value),
    BackendExited(BackendId),
    RetryRequest(BackendId, InFlightRequest),
    Tick,
    SignalStop,
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
    empty_since: Option<Instant>,
    backends_reused: u64,
    backends_hibernated: u64,
    backends_evicted: u64,
    tick_count: u64,
    shutdown: bool,
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
            empty_since: None,
            backends_reused: 0,
            backends_hibernated: 0,
            backends_evicted: 0,
            tick_count: 0,
            shutdown: false,
        }
    }

    fn handle(&mut self, ev: DaemonEvent) {
        match ev {
            DaemonEvent::NewLspClient {
                cwd,
                overrides,
                stream,
            } => self.on_new_client(cwd, overrides, stream),
            DaemonEvent::StatusRequest(stream) => self.on_status(stream),
            DaemonEvent::StopRequest(mut stream) => {
                self.shutdown_all_backends();
                let _ = wire::send(&mut stream, br#"{"ok":true}"#);
                self.shutdown = true;
            }
            DaemonEvent::ClientMessage(cid, msg) => self.on_client_message(cid, msg),
            DaemonEvent::ClientDisconnected(cid) => self.on_client_disconnected(cid),
            DaemonEvent::CapsProbed {
                client_id,
                ra_bin,
                result,
            } => self.on_caps_probed(client_id, ra_bin, result),
            DaemonEvent::BackendMessage(bid, msg) => self.on_backend_message(bid, msg),
            DaemonEvent::BackendExited(bid) => self.on_backend_exited(bid),
            DaemonEvent::RetryRequest(bid, req) => self.on_retry_request(bid, req),
            DaemonEvent::Tick => self.on_tick(),
            DaemonEvent::SignalStop => {
                self.shutdown_all_backends();
                self.shutdown = true;
            }
        }
    }

    fn on_new_client(&mut self, cwd: PathBuf, overrides: ClientEnvOverrides, stream: UnixStream) {
        self.empty_since = None;
        let id = self.next_client_id;
        self.next_client_id += 1;

        let Ok(read_stream) = stream.try_clone() else {
            return;
        };
        let mut write_stream = stream;
        let (out_tx, out_rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = mpsc::channel();
        thread::spawn(move || {
            for bytes in out_rx {
                if write_stream.write_all(&bytes).is_err() || write_stream.flush().is_err() {
                    break;
                }
            }
        });

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
                tx: out_tx,
                workspace_root: None,
                workspace_uri: None,
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
        let Some(bid) = self.ensure_client_active(cid) else {
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

        self.dispatch_to_backend(bid, cid, msg);
    }

    fn on_client_initialize(&mut self, cid: ClientId, msg: Value) {
        let Some(req_id) = msg.get("id").cloned() else {
            return;
        };
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

        let ra_bin = if let Some(explicit) = overrides.rust_analyzer_bin.as_deref() {
            Ok(fs::canonicalize(explicit).unwrap_or_else(|_| explicit.to_path_buf()))
        } else {
            resolve_real_ra(&workspace_root)
        };
        let ra_bin = match ra_bin {
            Ok(p) => p,
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
        let rustc_bin = resolve_rustc(&workspace_root);
        let share_worktrees = overrides
            .share_worktrees
            .unwrap_or(self.cfg.share_worktrees);
        let group = if share_worktrees {
            git_common_dir(&workspace_root).unwrap_or_else(|| workspace_root.clone())
        } else {
            workspace_root.clone()
        };
        let key = BackendKey {
            group,
            ra_bin: ra_bin.clone(),
            rustc_bin,
        };
        let init_options = params
            .get("initializationOptions")
            .cloned()
            .filter(Value::is_object)
            .unwrap_or_else(|| Value::Object(Map::new()));
        let supports_progress = params
            .pointer("/capabilities/window/workDoneProgress")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let lazy = overrides.lazy.unwrap_or(self.cfg.lazy);

        if let Some(c) = self.clients.get_mut(&cid) {
            c.workspace_root = Some(workspace_root);
            c.workspace_uri = Some(workspace_uri);
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
            let cache = self.cfg.cache_dir.clone();
            let tx = self.tx.clone();
            thread::spawn(move || {
                let result =
                    probe_ra_capabilities(&cache, &ra_bin, cid).map_err(|e| format!("{e:#}"));
                let _ = tx.send(DaemonEvent::CapsProbed {
                    client_id: cid,
                    ra_bin,
                    result,
                });
            });
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

    fn on_caps_probed(&mut self, cid: ClientId, ra_bin: PathBuf, result: Result<Value, String>) {
        match result {
            Ok(caps) => {
                self.caps_cache.insert(ra_bin, caps.clone());
                if let Some(c) = self.clients.get_mut(&cid)
                    && let Some(req_id) = c.waiting_init_req.take()
                {
                    c.send(&json!({ "jsonrpc": "2.0", "id": req_id, "result": caps }));
                }
            }
            Err(err) => {
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

    fn ensure_client_active(&mut self, cid: ClientId) -> Option<BackendId> {
        let (key, ws_root, ws_uri, init_opts, overrides, was_active, existing_bid) = {
            let c = self.clients.get(&cid)?;
            (
                c.backend_key.clone()?,
                c.workspace_root.clone()?,
                c.workspace_uri.clone()?,
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
                b.last_activity = Instant::now();
                if let Some(f) = b.loaded_folders.get_mut(&ws_root) {
                    f.last_active = Instant::now();
                }
            }
            return Some(bid);
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

        if let Some(c) = self.clients.get_mut(&cid) {
            c.backend_id = Some(bid);
            c.active = true;
        }

        if !spawned {
            // Attach this client's worktree folder if not already loaded in the shared backend.
            let need_add = self
                .backends
                .get(&bid)
                .is_some_and(|b| !b.loaded_folders.contains_key(&ws_root));
            if need_add && let Some(b) = self.backends.get_mut(&bid) {
                let now = Instant::now();
                b.loaded_folders.insert(
                    ws_root.clone(),
                    FolderState {
                        uri: ws_uri.clone(),
                        added_at: now,
                        saw_busy: false,
                        ready: false,
                        last_active: now,
                    },
                );
                b.quiescent = false;
                b.last_activity = now;
                let name = ws_root
                    .file_name()
                    .and_then(OsStr::to_str)
                    .unwrap_or("workspace");
                let add_notif = json!({
                    "jsonrpc": "2.0",
                    "method": "workspace/didChangeWorkspaceFolders",
                    "params": {
                        "event": {
                            "added": [{ "uri": ws_uri, "name": name }],
                            "removed": []
                        }
                    }
                });
                if b.ready {
                    b.send(&add_notif);
                } else {
                    b.pending_raw.push_back(add_notif);
                }
            }
        }
        // Replay any open documents this dormant/hibernated client already holds
        // (both when attaching to an existing backend and when waking a fresh one).
        self.replay_client_docs(bid, cid);
        Some(bid)
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

        let target_dir = self.cfg.backend_target_dir(&key, overrides);
        let ra_settings = self.cfg.merged_ra_settings(&key, client_opts, overrides);

        let log_path = self
            .cfg
            .cache_dir
            .join("rust-analyzer")
            .join(format!("backend-{bid}.log"));
        let stderr = File::create(&log_path).map_or_else(|_| Stdio::null(), Stdio::from);

        let mut cmd = Command::new(&key.ra_bin);
        cmd.current_dir(initial_ws)
            .env("JATO_RA_PASSTHROUGH", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .process_group(0);
        if env::var_os("MALLOC_ARENA_MAX").is_none() {
            cmd.env("MALLOC_ARENA_MAX", "2");
        }
        if let Some(tc) = overrides.rustup_toolchain.as_deref() {
            cmd.env("RUSTUP_TOOLCHAIN", tc);
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "jato rust-analyzer: failed to spawn {}: {e}",
                    key.ra_bin.display()
                );
                return None;
            }
        };
        let pid = child.id();
        let mut stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;

        let (out_tx, out_rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = mpsc::channel();
        thread::spawn(move || {
            for bytes in out_rx {
                if stdin.write_all(&bytes).is_err() || stdin.flush().is_err() {
                    break;
                }
            }
        });

        let ev_tx = self.tx.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(Some(msg)) = read_lsp_message(&mut reader) {
                if ev_tx.send(DaemonEvent::BackendMessage(bid, msg)).is_err() {
                    break;
                }
            }
            let _ = ev_tx.send(DaemonEvent::BackendExited(bid));
        });

        let now = Instant::now();
        let mut loaded_folders = HashMap::new();
        loaded_folders.insert(
            initial_ws.to_path_buf(),
            FolderState {
                uri: initial_uri.to_owned(),
                added_at: now,
                saw_busy: false,
                ready: false,
                last_active: now,
            },
        );

        let name = initial_ws
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("workspace");
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
            child,
            pid,
            tx: out_tx,
            target_dir,
            ra_settings,
            init_result: None,
            ready: false,
            quiescent: false,
            loaded_folders,
            open_docs: HashMap::new(),
            next_req_id: 2,
            in_flight: HashMap::new(),
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
        let Some(b) = self.backends.get_mut(&bid) else {
            return;
        };
        if let Some(existing) = b.open_docs.get_mut(uri) {
            existing.holders.insert(cid);
            if existing.text != text {
                existing.version += 1;
                existing.text = text.to_owned();
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
        b.open_docs.insert(
            uri.to_owned(),
            BackendDoc {
                holders,
                version: 1,
                text: text.to_owned(),
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
                    let shared = doc.holders.len() > 1;
                    if let Some(full) = full_text {
                        doc.text = full;
                    } else if let Some(changes) = msg
                        .pointer("/params/contentChanges")
                        .and_then(Value::as_array)
                    {
                        apply_content_changes(&mut doc.text, changes);
                    }
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
        if msg.get("id").is_some()
            && let Some(ws) = client_root.as_ref()
            && let Some(folder) = b.loaded_folders.get(ws)
            && !folder.ready
            && folder.added_at.elapsed() < WORKSPACE_READY_WAIT
        {
            b.pending_folder_ready.push_back(QueuedReadyMsg {
                client_id: cid,
                workspace_root: ws.clone(),
                msg,
            });
            return;
        }

        Self::send_request_or_notif(b, cid, msg);
    }

    fn send_request_or_notif(b: &mut BackendState, cid: ClientId, mut msg: Value) {
        if let Some(client_req_id) = msg.get("id").cloned() {
            let backend_id = b.next_req_id;
            b.next_req_id += 1;
            let method = msg
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let original_msg = msg.clone();
            msg["id"] = json!(backend_id);
            b.in_flight.insert(
                backend_id,
                InFlightRequest {
                    client_id: cid,
                    client_req_id,
                    method,
                    original_msg,
                    retries_left: MAX_CONTENT_MODIFIED_RETRIES,
                    cancelled: false,
                },
            );
            b.send(&msg);
        } else {
            b.send(&msg);
        }
    }

    fn release_doc_holder(&mut self, bid: BackendId, cid: ClientId, uri: &str) {
        let Some(b) = self.backends.get_mut(&bid) else {
            return;
        };
        let should_close = match b.open_docs.get_mut(uri) {
            Some(doc) => {
                doc.holders.remove(&cid);
                doc.holders.is_empty()
            }
            None => false,
        };
        if should_close {
            b.open_docs.remove(uri);
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
        let found = b.in_flight.iter_mut().find_map(|(&backend_id, req)| {
            if req.client_id == cid && &req.client_req_id == cancel_id {
                req.cancelled = true;
                Some(backend_id)
            } else {
                None
            }
        });
        if let Some(backend_id) = found {
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

    fn on_backend_message(&mut self, bid: BackendId, mut msg: Value) {
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
            if req.cancelled || !self.clients.contains_key(&req.client_id) {
                return;
            }
            // Retry transient `ContentModified` (-32801) caused by sibling worktree reload.
            let err_code = msg.pointer("/error/code").and_then(Value::as_i64);
            if matches!(err_code, Some(-32801)) && req.retries_left > 0 {
                req.retries_left -= 1;
                let tx = self.tx.clone();
                thread::spawn(move || {
                    thread::sleep(RETRY_DELAY);
                    let _ = tx.send(DaemonEvent::RetryRequest(bid, req));
                });
                return;
            }
            let other_roots = self
                .backends
                .get(&bid)
                .map(|b| {
                    let ws = self
                        .clients
                        .get(&req.client_id)
                        .and_then(|c| c.workspace_root.as_deref());
                    b.other_root_uris(ws)
                })
                .unwrap_or_default();
            if let Some(res) = msg.get_mut("result") {
                filter_response_for_workspace(&req.method, res, &other_roots);
            }
            msg["id"] = req.client_req_id;
            let now = Instant::now();
            if let Some(b) = self.backends.get_mut(&bid) {
                b.last_activity = now;
            }
            if let Some(c) = self.clients.get_mut(&req.client_id) {
                c.last_activity = now;
                c.send(&msg);
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
                    b.quiescent = quiescent;
                    if !quiescent {
                        for folder in b.loaded_folders.values_mut() {
                            if !folder.ready {
                                folder.saw_busy = true;
                            }
                        }
                    } else {
                        for folder in b.loaded_folders.values_mut() {
                            if !folder.ready && folder.saw_busy {
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
                    let matches = c.open_docs.contains_key(uri)
                        || c.workspace_uri
                            .as_deref()
                            .is_some_and(|root| uri_in_root(uri, root));
                    if matches {
                        c.send(&msg);
                    }
                }
            }
            Some("$/progress") => {
                if msg.pointer("/params/value/kind").and_then(Value::as_str) == Some("begin")
                    && let Some(b) = self.backends.get_mut(&bid)
                {
                    for folder in b.loaded_folders.values_mut() {
                        if !folder.ready {
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
            return;
        };
        let (ra_bin, pending) = {
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
            for raw in pending_raw {
                b.send(&raw);
            }
            let pending: Vec<_> = b.pending_init.drain(..).collect();
            (b.key.ra_bin.clone(), pending)
        };
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
        let queued: Vec<QueuedReadyMsg> = {
            let Some(b) = self.backends.get_mut(&bid) else {
                return;
            };
            let mut ready = Vec::new();
            let mut keep = VecDeque::new();
            while let Some(item) = b.pending_folder_ready.pop_front() {
                let is_ready = b
                    .loaded_folders
                    .get(&item.workspace_root)
                    .is_none_or(|f| f.ready || f.added_at.elapsed() >= WORKSPACE_READY_WAIT);
                if is_ready {
                    ready.push(item);
                } else {
                    keep.push_back(item);
                }
            }
            b.pending_folder_ready = keep;
            ready
        };
        for item in queued {
            if let Some(b) = self.backends.get_mut(&bid) {
                Self::send_request_or_notif(b, item.client_id, item.msg);
            }
        }
    }

    fn on_retry_request(&mut self, bid: BackendId, req: InFlightRequest) {
        if req.cancelled || !self.clients.contains_key(&req.client_id) {
            return;
        }
        let Some(b) = self.backends.get_mut(&bid) else {
            if let Some(c) = self.clients.get(&req.client_id) {
                c.send(&json!({
                    "jsonrpc": "2.0",
                    "id": req.client_req_id,
                    "error": { "code": -32603, "message": "rust-analyzer backend exited" }
                }));
            }
            return;
        };
        let backend_id = b.next_req_id;
        b.next_req_id += 1;
        let mut msg = req.original_msg.clone();
        msg["id"] = json!(backend_id);
        b.in_flight.insert(backend_id, req);
        b.send(&msg);
    }

    fn on_backend_exited(&mut self, bid: BackendId) {
        let Some(mut b) = self.backends.remove(&bid) else {
            return;
        };
        if self.backend_by_key.get(&b.key) == Some(&bid) {
            self.backend_by_key.remove(&b.key);
        }
        let _ = b.child.wait();

        // Fail any in-flight requests cleanly so clients never hang.
        for (_, req) in b.in_flight {
            if let Some(c) = self.clients.get(&req.client_id) {
                c.send(&json!({
                    "jsonrpc": "2.0",
                    "id": req.client_req_id,
                    "error": { "code": -32603, "message": "rust-analyzer backend exited unexpectedly" }
                }));
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
        b.send(
            &json!({ "jsonrpc": "2.0", "id": b.next_req_id, "method": "shutdown", "params": null }),
        );
        b.send(&json!({ "jsonrpc": "2.0", "method": "exit", "params": null }));
        drop(b.tx);
        let pid = b.pid;
        Some(thread::spawn(move || {
            let pgrp = pid as libc::pid_t;
            let deadline = Instant::now() + Duration::from_millis(1200);
            while Instant::now() < deadline {
                if matches!(b.child.try_wait(), Ok(Some(_))) {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            unsafe {
                libc::kill(-pgrp, libc::SIGTERM);
            }
            let term_deadline = Instant::now() + Duration::from_millis(300);
            while Instant::now() < term_deadline {
                if matches!(b.child.try_wait(), Ok(Some(_))) && unsafe { libc::kill(-pgrp, 0) } != 0
                {
                    return;
                }
                thread::sleep(Duration::from_millis(25));
            }
            unsafe {
                libc::kill(-pgrp, libc::SIGKILL);
            }
            let _ = b.child.kill();
            let _ = b.child.wait();
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
        let pids: Vec<u32> = self.backends.values().map(|b| b.pid).collect();
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
        // Never evict a backend with in-flight requests; prefer 0 active clients,
        // then oldest last_activity.
        self.backends
            .values()
            .filter(|b| b.in_flight.is_empty() && b.pending_folder_ready.is_empty())
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
        b.in_flight.values().any(|r| r.client_id == cid)
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
            let active_ws: HashSet<&Path> = self
                .clients
                .values()
                .filter(|c| c.active && c.backend_id == Some(bid))
                .filter_map(|c| c.workspace_root.as_deref())
                .collect();

            if active_ws.is_empty() {
                let grace = if self.clients.values().any(|c| c.backend_id == Some(bid)) {
                    if self.cfg.idle_timeout.is_zero() {
                        continue;
                    }
                    self.cfg.idle_timeout
                } else if self.cfg.idle_timeout.is_zero() {
                    ORPHAN_BACKEND_GRACE
                } else {
                    self.cfg.idle_timeout.min(ORPHAN_BACKEND_GRACE)
                };
                if b.in_flight.is_empty() && now.saturating_duration_since(b.last_activity) >= grace
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
                let Ok(bytes) = wire::receive(&mut stream) else {
                    return;
                };
                let Ok(hs) = serde_json::from_slice::<Handshake>(&bytes) else {
                    return;
                };
                let event = match hs {
                    Handshake::Status => DaemonEvent::StatusRequest(stream),
                    Handshake::Stop => DaemonEvent::StopRequest(stream),
                    Handshake::Lsp { cwd, overrides, .. } => DaemonEvent::NewLspClient {
                        cwd,
                        overrides,
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
    for event in rx {
        daemon.handle(event);
        if daemon.shutdown {
            break;
        }
    }
    daemon.shutdown_all_backends();
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
}
