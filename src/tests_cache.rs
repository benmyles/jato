//! Observed hermetic test caching and parallel workspace test execution.
//!
//! When `jato test` runs compiled test binaries, Cargo invokes `jato __test`
//! via `CARGO_TARGET_<TRIPLE>_RUNNER`, passing Cargo's exact environment,
//! working directory, binary path, and arguments. Each test binary run is
//! looked up by the binary's content digest, arguments, and filtered
//! environment, and verified against every file, directory, symlink, and
//! subprocess observed during the earlier run. Passing, hermetic,
//! side-effect-free test runs are cached and reused across re-runs and
//! worktrees; cache misses across a workspace execute in parallel across
//! available cores while presenting deterministic Cargo-formatted output.
use crate::cas::Store;
use crate::command::{
    self, Outcome, Ran, decode_detail, encode_detail, exit_code, field, harmless_socket, log,
    own_environment, tee, umask,
};
use crate::manifest::{Env, Here, Input, Manifest, Place, State, path_dirs, which};
use crate::observe::{self, Kind, Report, Stamp};
use crate::roots::{self, Roots};
use crate::scan::Digest;
use crate::wire::{self, Reader, Writer};
use anyhow::{Context, Result};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Instant;

const TEST_EPOCH: &str = "jato test result v1";
const DONE: u8 = 0;
const DECLINE: u8 = 1;
const QUEUED: u8 = 2;
const RUN_SYNC: u8 = 3;

fn os(bytes: &[u8]) -> OsString {
    OsString::from_vec(bytes.to_vec())
}

fn address(name: &OsStr) -> io::Result<SocketAddr> {
    SocketAddr::from_abstract_name(name.as_bytes())
}

/// Detect the target triple for `cargo test`: `--target`, `CARGO_BUILD_TARGET`,
/// or the host triple from `cargo -vV`.
fn target_triple(args: &[OsString]) -> Option<String> {
    let end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let mut i = 0;
    let mut toolchain: Option<&OsStr> = None;
    while i < end {
        let a = args[i].to_str();
        match a {
            Some(s) if s.starts_with('+') && toolchain.is_none() => toolchain = Some(&args[i]),
            Some("--target") if i + 1 < end => {
                return args[i + 1].to_str().map(str::to_owned);
            }
            Some(s) if s.starts_with("--target=") => {
                return Some(s["--target=".len()..].to_owned());
            }
            _ => (),
        }
        i += 1;
    }
    if let Ok(target) = env::var("CARGO_BUILD_TARGET")
        && !target.trim().is_empty()
    {
        return Some(target.trim().to_owned());
    }
    if let Some(tc) = toolchain {
        return cargo_host_triple(Some(tc));
    }
    static HOST: OnceLock<Option<String>> = OnceLock::new();
    HOST.get_or_init(|| cargo_host_triple(None)).clone()
}

fn cargo_host_triple(toolchain: Option<&OsStr>) -> Option<String> {
    let (cargo, unset_cargo) = crate::cargo_program().ok()?;
    let mut cmd = Command::new(cargo);
    if unset_cargo {
        cmd.env_remove("CARGO");
    }
    if let Some(tc) = toolchain {
        cmd.arg(tc);
    }
    let out = cmd
        .arg("-vV")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find_map(|line| line.strip_prefix("host:"))
        .map(|s| s.trim().to_owned())
}

fn runner_env_name(triple: &str) -> String {
    let norm: String = triple
        .chars()
        .map(|c| {
            if c == '-' || c == '.' {
                '_'
            } else {
                c.to_ascii_uppercase()
            }
        })
        .collect();
    format!("CARGO_TARGET_{norm}_RUNNER")
}

/// Whether `value` starts with a known cache launcher (e.g. `kache test-runner`)
/// that Jato should step around.
fn is_launcher_runner(value: &str) -> bool {
    let first = value
        .trim()
        .trim_start_matches('[')
        .split(|c: char| c.is_whitespace() || c == ',' || c == '"')
        .find(|s| !s.is_empty());
    let Some(first) = first else {
        return false;
    };
    let name = Path::new(first)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(first);
    crate::LAUNCHERS.contains(&name)
}

/// Whether the user has configured a real (non-cache-launcher) test runner
/// for `triple` in the environment or in Cargo's configuration files.
fn has_custom_runner(triple: &str, runner_var: &str, cwd: &Path) -> bool {
    if let Ok(val) = env::var(runner_var) {
        return !val.trim().is_empty() && !is_launcher_runner(&val);
    }
    let mut files: Vec<PathBuf> = cwd
        .ancestors()
        .map(|dir| dir.join(".cargo/config.toml"))
        .collect();
    let home = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| Path::new(&h).join(".cargo")));
    files.extend(home.map(|h| h.join("config.toml")));
    let target_table = format!("target.{triple}");
    for file in files {
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        let mut in_matching = false;
        for line in text.lines().map(str::trim) {
            if let Some(header) = line.strip_prefix('[') {
                let table = header.split(']').next().unwrap_or_default().trim();
                in_matching = table == target_table || table.starts_with("target.'cfg(");
                continue;
            }
            if !in_matching {
                continue;
            }
            if let Some((k, v)) = line.split_once('=')
                && k.trim() == "runner"
            {
                let v = v.split_once(" #").map_or(v.trim(), |(v, _)| v.trim());
                let unquoted = v.trim_matches('"');
                if !unquoted.is_empty() && !is_launcher_runner(unquoted) {
                    return true;
                }
            }
        }
    }
    false
}

/// True if test arguments after `--` include `--list` (enumeration only).
fn has_list_flag(args: &[OsString]) -> bool {
    let Some(dash) = args.iter().position(|a| a == "--") else {
        return false;
    };
    args[dash + 1..].iter().any(|a| a == "--list")
}

/// Per-project test concurrency settings read from `Cargo.toml`
/// (`[workspace.metadata.jato]` or `[package.metadata.jato]`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ProjectTestConfig {
    /// Maximum test binaries to run at once (`0` or `1` runs one at a time).
    parallel_tests: Option<usize>,
    /// Default `RUST_TEST_THREADS` per test binary (`0` leaves libtest's default).
    test_threads: Option<usize>,
}

impl ProjectTestConfig {
    fn or(self, fallback: Self) -> Self {
        Self {
            parallel_tests: self.parallel_tests.or(fallback.parallel_tests),
            test_threads: self.test_threads.or(fallback.test_threads),
        }
    }
}

/// Effective concurrency settings for a `jato test` invocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TestConcurrency {
    /// `Some(n)` (`n >= 2`) runs up to `n` test binaries concurrently;
    /// `None` runs test binaries synchronously one at a time.
    parallel_workers: Option<usize>,
    /// `Some(n)` (`n >= 1`) sets `RUST_TEST_THREADS=n` for test execution;
    /// `None` leaves `RUST_TEST_THREADS` untouched.
    rust_test_threads: Option<usize>,
}

const MAX_EXPLICIT_PARALLEL_TESTS: usize = 64;

/// Locate an explicit `--manifest-path` (if passed) and the starting directory
/// for Cargo manifest discovery (honoring `-C` and `--manifest-path`).
fn manifest_location(args: &[OsString], cwd: &Path) -> (Option<PathBuf>, PathBuf) {
    let end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let mut base = cwd.to_path_buf();
    let mut manifest = None;
    let mut i = 0;
    while i < end {
        match args[i].to_str() {
            Some("-C") if i + 1 < end => {
                base = base.join(&args[i + 1]);
                i += 2;
                continue;
            }
            Some("--manifest-path") if i + 1 < end => {
                manifest = Some(base.join(&args[i + 1]));
                i += 2;
                continue;
            }
            Some(s) if s.starts_with("--manifest-path=") => {
                manifest = Some(base.join(&s["--manifest-path=".len()..]));
            }
            _ => (),
        }
        i += 1;
    }
    let start_dir = manifest
        .as_deref()
        .and_then(Path::parent)
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or(base);
    let start_dir = fs::canonicalize(&start_dir).unwrap_or(start_dir);
    (manifest, start_dir)
}

/// Strip a `#` comment outside quotes while tracking multi-line TOML strings
/// (`"""` and `'''`) across lines.
fn strip_toml_line(line: &str, multiline: &mut Option<u8>) -> String {
    let bytes = line.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        if let Some(m) = *multiline {
            if m == b'"' && bytes[i] == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if i + 2 < bytes.len() && bytes[i] == m && bytes[i + 1] == m && bytes[i + 2] == m {
                *multiline = None;
                i += 3;
                continue;
            }
            i += 1;
            continue;
        }
        if let Some(q) = quote {
            let ch = line[i..].chars().next().unwrap();
            out.push(ch);
            if q == b'"' && bytes[i] == b'\\' && i + 1 < bytes.len() {
                let next = line[i + 1..].chars().next().unwrap();
                out.push(next);
                i += 1 + next.len_utf8();
                continue;
            }
            if bytes[i] == q {
                quote = None;
            }
            i += ch.len_utf8();
            continue;
        }
        match bytes[i] {
            b'#' => break,
            b'"' | b'\''
                if i + 2 < bytes.len() && bytes[i + 1] == bytes[i] && bytes[i + 2] == bytes[i] =>
            {
                *multiline = Some(bytes[i]);
                i += 3;
            }
            b'"' | b'\'' => {
                quote = Some(bytes[i]);
                out.push(bytes[i] as char);
                i += 1;
            }
            _ => {
                let ch = line[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

/// Normalize a dotted TOML key path (`a . "b" . 'c'` -> `a.b.c`).
fn normalize_toml_path(raw: &str) -> String {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for ch in raw.chars() {
        if let Some(q) = quote {
            if escaped {
                cur.push(ch);
                escaped = false;
            } else if q == '"' && ch == '\\' {
                escaped = true;
            } else if ch == q {
                quote = None;
            } else {
                cur.push(ch);
            }
            continue;
        }
        match ch {
            '"' | '\'' => quote = Some(ch),
            '.' => {
                let t = cur.trim();
                if !t.is_empty() {
                    parts.push(t.to_owned());
                }
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    let t = cur.trim();
    if !t.is_empty() {
        parts.push(t.to_owned());
    }
    parts.join(".")
}

/// Split `s` at the first `=` outside quotes and braces.
fn split_key_value(s: &str) -> Option<(&str, &str)> {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (idx, ch) in s.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if q == '"' && ch == '\\' {
                escaped = true;
            } else if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => quote = Some(ch),
            '=' => return Some((s[..idx].trim(), s[idx + 1..].trim())),
            _ => (),
        }
    }
    None
}

/// Split top-level comma-separated entries inside an inline TOML table `{ ... }`.
fn split_inline_entries(body: &str) -> Vec<&str> {
    let mut entries = Vec::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut depth = 0usize;
    let mut start = 0usize;
    for (idx, ch) in body.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if q == '"' && ch == '\\' {
                escaped = true;
            } else if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => quote = Some(ch),
            '{' | '[' => depth += 1,
            '}' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                let item = body[start..idx].trim();
                if !item.is_empty() {
                    entries.push(item);
                }
                start = idx + 1;
            }
            _ => (),
        }
    }
    let last = body[start..].trim();
    if !last.is_empty() {
        entries.push(last);
    }
    entries
}

/// Parse a non-negative TOML integer (accepting optional `_` separators and,
/// when `allow_false` is true, `false` as `0`).
fn parse_toml_usize(raw: &str, allow_false: bool) -> Option<usize> {
    let v = raw.trim();
    if allow_false && v == "false" {
        return Some(0);
    }
    if v.is_empty() || v.starts_with('-') {
        return None;
    }
    let digits = v.strip_prefix('+').unwrap_or(v);
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.bytes().all(|b| b.is_ascii_digit() || b == b'_')
    {
        return None;
    }
    let cleaned: String = digits.chars().filter(|&c| c != '_').collect();
    cleaned.parse().ok()
}

fn apply_toml_entry(
    full_key: &str,
    value: &str,
    workspace: &mut ProjectTestConfig,
    package: &mut ProjectTestConfig,
) {
    let val = value.trim();
    if let Some(inner) = val.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
        for item in split_inline_entries(inner) {
            if let Some((k, v)) = split_key_value(item) {
                let sub = normalize_toml_path(k);
                if !sub.is_empty() {
                    let nested = format!("{full_key}.{sub}");
                    apply_toml_entry(&nested, v, workspace, package);
                }
            }
        }
        return;
    }
    let (target, field) = if let Some(rest) = full_key.strip_prefix("workspace.metadata.jato.") {
        (workspace, rest)
    } else if let Some(rest) = full_key.strip_prefix("package.metadata.jato.") {
        (package, rest)
    } else {
        return;
    };
    match field {
        "parallel-tests" | "parallel_tests" => {
            if let Some(n) = parse_toml_usize(val, true) {
                target.parallel_tests = Some(n);
            } else {
                eprintln!(
                    "jato: ignoring invalid {full_key} = {val} (expected a non-negative integer)"
                );
            }
        }
        "test-threads" | "test_threads" => {
            if let Some(n) = parse_toml_usize(val, false) {
                target.test_threads = Some(n);
            } else {
                eprintln!(
                    "jato: ignoring invalid {full_key} = {val} (expected a non-negative integer)"
                );
            }
        }
        _ => (),
    }
}

/// Extract `(workspace, package)` Jato test concurrency settings from a
/// `Cargo.toml` file's text.
fn parse_manifest_test_config(text: &str) -> (ProjectTestConfig, ProjectTestConfig) {
    let mut workspace = ProjectTestConfig::default();
    let mut package = ProjectTestConfig::default();
    let mut section = String::new();
    let mut multiline = None;
    for raw_line in text.lines() {
        let stripped = strip_toml_line(raw_line, &mut multiline);
        let line = stripped.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("[[") {
            let header = rest.split("]]").next().unwrap_or_default();
            section = format!("[[{}]]", normalize_toml_path(header));
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            let header = rest.split(']').next().unwrap_or_default();
            section = normalize_toml_path(header);
            continue;
        }
        if section.starts_with("[[") {
            continue;
        }
        if let Some((k, v)) = split_key_value(line) {
            let key = normalize_toml_path(k);
            if key.is_empty() {
                continue;
            }
            let full_key = if section.is_empty() {
                key
            } else {
                format!("{section}.{key}")
            };
            apply_toml_entry(&full_key, v, &mut workspace, &mut package);
        }
    }
    (workspace, package)
}

/// Load project test concurrency configuration from the targeted package and
/// enclosing workspace `Cargo.toml`.
///
/// Precedence: member `[package.metadata.jato]` > workspace
/// `[workspace.metadata.jato]` > workspace `[package.metadata.jato]`.
fn load_project_test_config(args: &[OsString], cwd: &Path) -> ProjectTestConfig {
    let (explicit_manifest, start_dir) = manifest_location(args, cwd);
    let nearest_manifest = explicit_manifest.filter(|p| p.is_file()).or_else(|| {
        start_dir
            .ancestors()
            .map(|dir| dir.join("Cargo.toml"))
            .find(|p| p.is_file())
    });
    let workspace_manifest = roots::workspace_above(&start_dir).map(|dir| dir.join("Cargo.toml"));

    let mut member_cfg = ProjectTestConfig::default();
    let mut workspace_cfg = ProjectTestConfig::default();

    let same_file = |a: &Path, b: &Path| {
        a == b
            || fs::canonicalize(a)
                .ok()
                .zip(fs::canonicalize(b).ok())
                .is_some_and(|(ca, cb)| ca == cb)
    };

    if let Some(ws_path) = workspace_manifest.as_deref()
        && let Ok(text) = fs::read_to_string(ws_path)
    {
        let (ws, pkg) = parse_manifest_test_config(&text);
        workspace_cfg = ws.or(pkg);
    }

    if let Some(pkg_path) = nearest_manifest.as_deref()
        && workspace_manifest
            .as_deref()
            .is_none_or(|ws_path| !same_file(pkg_path, ws_path))
        && let Ok(text) = fs::read_to_string(pkg_path)
    {
        let (ws, pkg) = parse_manifest_test_config(&text);
        member_cfg = pkg.or(ws);
    }

    member_cfg.or(workspace_cfg)
}

fn parse_env_usize(name: &str, raw: Option<&OsStr>) -> Option<usize> {
    let raw = raw?;
    let text = raw.to_string_lossy();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.parse::<usize>() {
        Ok(n) => Some(n),
        Err(_) => {
            eprintln!(
                "jato: ignoring invalid {name}={trimmed:?} (expected a non-negative integer)"
            );
            None
        }
    }
}

/// Inspect test-binary flags after `--` for streaming (`--nocapture`) and
/// explicit `--test-threads` settings.
fn cli_test_flags(args: &[OsString]) -> (bool, bool, bool) {
    let Some(dash) = args.iter().position(|a| a == "--") else {
        return (false, false, false);
    };
    let tail = &args[dash + 1..];
    let mut nocapture = false;
    let mut has_test_threads = false;
    let mut single_thread = false;
    for (i, arg) in tail.iter().enumerate() {
        let s = arg.to_str().unwrap_or_default();
        if s == "--nocapture" || s == "--no-capture" {
            nocapture = true;
        } else if let Some(val) = s.strip_prefix("--test-threads=") {
            has_test_threads = true;
            if val == "1" {
                single_thread = true;
            }
        } else if s == "--test-threads" {
            has_test_threads = true;
            if tail.get(i + 1).is_some_and(|v| v == "1") {
                single_thread = true;
            }
        }
    }
    (nocapture, has_test_threads, single_thread)
}

fn default_test_workers() -> usize {
    let workers = crate::jobs::limit();
    if workers == 0 {
        std::thread::available_parallelism().map_or(8, |n| n.get())
    } else {
        workers
    }
    .clamp(1, 32)
}

/// Resolve effective parallel binary count and per-binary `RUST_TEST_THREADS`
/// from CLI args, environment variables, and `Cargo.toml` metadata.
fn resolve_test_concurrency(
    args: &[OsString],
    project: ProjectTestConfig,
    env_parallel: Option<&OsStr>,
    env_threads: Option<&OsStr>,
    env_rust_test_threads: Option<&OsStr>,
    default_workers: usize,
) -> TestConcurrency {
    let (nocapture, cli_has_threads, cli_single_thread) = cli_test_flags(args);

    let parsed_env_threads = parse_env_usize("JATO_TEST_THREADS", env_threads);
    let rust_test_threads = if cli_has_threads {
        None
    } else if let Some(n) = parsed_env_threads {
        (n > 0).then_some(n)
    } else if env_rust_test_threads.is_some_and(|v| !v.to_string_lossy().trim().is_empty()) {
        None
    } else {
        project.test_threads.filter(|&n| n > 0)
    };

    let parsed_env_parallel = parse_env_usize("JATO_PARALLEL_TESTS", env_parallel);
    let parallel_workers = if nocapture {
        None
    } else if let Some(n) = parsed_env_parallel {
        if n <= 1 {
            None
        } else {
            Some(n.min(MAX_EXPLICIT_PARALLEL_TESTS))
        }
    } else if cli_single_thread {
        None
    } else if let Some(cap) = project.parallel_tests {
        let w = default_workers.min(cap);
        (w > 1).then_some(w)
    } else {
        (default_workers > 1).then_some(default_workers)
    };

    TestConcurrency {
        parallel_workers,
        rust_test_threads,
    }
}

// ---- test request, recipe, lookup, execution, recording -------------------

struct TestRequest {
    cwd: PathBuf,
    binary: PathBuf,
    args: Vec<OsString>,
    env: Env,
}

fn excluded_env(name: &[u8]) -> bool {
    name.starts_with(b"JATO_")
        || name == b"LD_PRELOAD"
        || name == b"PWD"
        || name == b"OLDPWD"
        || name == b"SHLVL"
        || name == b"_"
        || name == b"CARGO_MAKEFLAGS"
        || name == b"MAKEFLAGS"
        || name == b"MFLAGS"
        || name == b"RUSTC_WRAPPER"
        || name == crate::manifest::LIBRARY_PATH
        || (name.starts_with(b"CARGO_TARGET_") && name.ends_with(b"_RUNNER"))
}

fn test_roots(req: &TestRequest) -> Roots {
    let named = ["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"]
        .iter()
        .find_map(|name| req.env.get(OsStr::new(name)))
        .map(|path| req.cwd.join(path));
    let manifest_dir = req
        .env
        .get(OsStr::new("CARGO_MANIFEST_DIR"))
        .map(PathBuf::from);
    Roots::of_script(
        &req.cwd,
        manifest_dir.as_deref(),
        Some(&req.binary),
        named.as_deref(),
    )
}

fn test_recipe(req: &TestRequest, roots: &Roots, binary_digest: &Digest) -> Digest {
    let mut hash = blake3::Hasher::new_derive_key(TEST_EPOCH);
    hash.update(binary_digest);
    field(&mut hash, &roots.mark_os(req.cwd.as_os_str()));
    field(&mut hash, &(req.args.len() as u64).to_le_bytes());
    for arg in &req.args {
        field(&mut hash, &roots.mark_os(arg));
    }
    field(&mut hash, b"environment");
    for (name, value) in &req.env {
        if !excluded_env(name.as_bytes()) {
            field(&mut hash, name.as_bytes());
            field(&mut hash, &roots.mark_os(value));
        }
    }
    field(&mut hash, &umask().to_le_bytes());
    *hash.finalize().as_bytes()
}

fn test_label(binary: &Path) -> String {
    let name = binary.file_name().and_then(OsStr::to_str).unwrap_or("test");
    format!("test:{name}")
}

/// Look up a cached passing test run.
fn lookup_test(
    req: &TestRequest,
    roots: &Roots,
    recipe: &Digest,
    store: &mut Store,
) -> Result<Option<Outcome>> {
    let path = path_dirs(&req.cwd, &req.env);
    for attempt in 0..2 {
        store.begin_read();
        let candidates = store.candidates(recipe)?;
        let mut here = Here::new(roots, &req.env, &path, None, false);
        let mut forget = Vec::new();
        let mut matched = None;
        for candidate in &candidates {
            let stored = store.record(candidate).unwrap_or_default();
            let manifest = match Manifest::decode_verified(&stored, recipe, &candidate.key) {
                Ok(m) => m,
                _ => {
                    forget.push(candidate.key);
                    continue;
                }
            };
            if manifest.holds(&mut here, store).is_err() {
                continue;
            }
            let mut damaged = false;
            let restored = store.restore(
                candidate,
                &manifest,
                roots,
                crate::cas::Placing::default(),
                &mut damaged,
            )?;
            let Some(restored) = restored else {
                if damaged {
                    forget.push(candidate.key);
                }
                continue;
            };
            let Some((code, stdout, stderr)) = decode_detail(&restored.detail) else {
                continue;
            };
            let (Some(stdout), Some(stderr)) = (roots.unmark(stdout), roots.unmark(stderr)) else {
                continue;
            };
            store.count("test_hit", 1);
            store.count("saved_ms", restored.cost_ms);
            log(&test_label(&req.binary), "hit");
            matched = Some(Outcome {
                code,
                stdout: stdout.into_owned(),
                stderr: stderr.into_owned(),
            });
            break;
        }
        store.end_read();
        for key in forget {
            store.forget(&key);
        }
        if matched.is_some() || attempt > 0 || !store.has_remote() {
            return Ok(matched);
        }
        let mut here = Here::new(roots, &req.env, &path, None, false);
        if !store.pull_remote(recipe, &mut here, roots, false) {
            return Ok(None);
        }
    }
    Ok(None)
}

/// Build a clean child `Command` for running the test binary directly,
/// stripping Jato's test session variables so nested Cargo calls inside tests
/// do not interfere with this session.
fn test_command(req: &TestRequest, runner_var: &str) -> Command {
    let mut cmd = Command::new(&req.binary);
    cmd.args(&req.args).current_dir(&req.cwd).env_clear();
    let stand_ins = req.env.get(OsStr::new(crate::tools::DIRECTORY));
    for (k, v) in &req.env {
        let kb = k.as_bytes();
        if kb == b"JATO_TEST_SESSION"
            || kb == b"JATO_TEST_PARALLEL"
            || kb == runner_var.as_bytes()
            || kb == crate::tools::DIRECTORY.as_bytes()
        {
            continue;
        }
        if kb == b"PATH"
            && let Some(dir) = stand_ins
        {
            cmd.env(k, crate::tools::without(v, dir));
            continue;
        }
        cmd.env(k, v);
    }
    cmd
}

/// Execute a test binary under observation. When `stream` is true, stdout and
/// stderr are streamed live to this process's stdout/stderr as well as captured.
fn execute_observed(
    req: &TestRequest,
    store: &Store,
    runner_var: &str,
    stream: bool,
) -> Result<(Ran, Report)> {
    let begun = Instant::now();
    let mut cmd = test_command(req, runner_var);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let config = observe::Config {
        fast: observe::fast_library(&store.dir),
        ignore: vec![store.dir.clone()],
        unlisted: Vec::new(),
        state: None,
        programs_in: None,
        mount: None,
    };
    let (mut child, session) = observe::spawn(&mut cmd, config)?;
    drop(cmd);
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let copy_out = std::thread::spawn(move || {
        if stream {
            tee(out, io::stdout())
        } else {
            tee(out, io::sink())
        }
    });
    let (stderr, streamed_err) = if stream {
        tee(err, io::stderr())
    } else {
        tee(err, io::sink())
    };
    let (stdout, streamed_out) = copy_out
        .join()
        .unwrap_or((None, Err(io::Error::other("output thread failed"))));
    if streamed_err.and(streamed_out).is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let report = session.finish();
    Ok((
        Ran {
            code: exit_code(status),
            success: status.success(),
            stdout,
            stderr,
            millis: begun.elapsed().as_millis() as u64,
        },
        report,
    ))
}

/// Execute a test binary directly without seccomp observation (used as a
/// fallback when a test binary or its child requires its own seccomp user
/// notification listener).
fn execute_direct(req: &TestRequest, runner_var: &str, stream: bool) -> Result<Ran> {
    let begun = Instant::now();
    let mut cmd = test_command(req, runner_var);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let copy_out = std::thread::spawn(move || {
        if stream {
            tee(out, io::stdout())
        } else {
            tee(out, io::sink())
        }
    });
    let (stderr, streamed_err) = if stream {
        tee(err, io::stderr())
    } else {
        tee(err, io::sink())
    };
    let (stdout, streamed_out) = copy_out
        .join()
        .unwrap_or((None, Err(io::Error::other("output thread failed"))));
    if streamed_err.and(streamed_out).is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    Ok(Ran {
        code: exit_code(status),
        success: status.success(),
        stdout,
        stderr,
        millis: begun.elapsed().as_millis() as u64,
    })
}

/// Record a passing, hermetic, side-effect-free test execution.
fn record_test(
    req: &TestRequest,
    roots: &Roots,
    recipe: &Digest,
    ran: &Ran,
    report: &Report,
    store: &mut Store,
) -> Result<(), String> {
    if !ran.success {
        return Err(format!("exited with status {}", ran.code));
    }
    if let Some(why) = &report.incomplete {
        return Err(why.clone());
    }
    if report.network {
        return Err("it used the network".into());
    }
    if report.survivors {
        return Err("a child process outlived the test".into());
    }
    if let Some((socket, caller)) = report.sockets.iter().find(|(s, _)| !harmless_socket(s)) {
        return Err(format!(
            "`{caller}` talked to a service at {}",
            socket.display()
        ));
    }
    let (Some(stdout), Some(stderr)) = (&ran.stdout, &ran.stderr) else {
        return Err("it printed more than can be kept".into());
    };
    let mut manifest = Manifest::default();
    let mut executed = Vec::new();
    let homes: Vec<PathBuf> = [roots::WORK, roots::TARGET]
        .iter()
        .filter_map(|root| roots.path(*root).map(Path::to_owned))
        .collect();
    let leads_to = |path: &Path| {
        homes
            .iter()
            .any(|place| place != path && place.starts_with(path))
    };
    let mut paths: Vec<_> = report.paths.iter().collect();
    paths.sort_by_key(|(path, _)| *path);
    for (path, access) in paths {
        // The test binary itself is already keyed by its content digest in `recipe`.
        if *path == req.binary {
            continue;
        }
        let now = fs::symlink_metadata(path);
        if access.written {
            // Temporary files created and removed before exit are harmless;
            // any file still on disk is a persistent side effect.
            if now.is_ok() {
                return Err(format!("it left {} on disk", path.display()));
            }
            if access.updated && !access.created {
                return Err(format!("it modified {}", path.display()));
            }
            continue;
        }
        if access.removed {
            if !access.created && access.kind != Kind::Absent {
                return Err(format!("it removed {}", path.display()));
            }
            continue;
        }
        if access.created {
            continue;
        }
        let passed = access.kind == Kind::Dir && access.listing.is_none() && access.link.is_none();
        if passed && leads_to(path) {
            continue;
        }
        let (root, rel) = roots.split(path);
        let state = match access.kind {
            Kind::Absent if access.appeared || (now.is_ok() && fs::metadata(path).is_ok()) => {
                return Err(format!("{} appeared while it ran", path.display()));
            }
            Kind::Absent => State::Absent,
            Kind::Dir => {
                let listed = access
                    .listing
                    .as_ref()
                    .map(|names| observe::listing_digest(names.clone()));
                State::Dir(listed)
            }
            Kind::File => {
                if path == Path::new("/etc/ld.so.cache") {
                    continue;
                }
                let first = access.stamp.unwrap();
                let Ok(meta) = fs::metadata(path) else {
                    return Err(format!("{} went away while it ran", path.display()));
                };
                let stamp = Stamp::of(&meta);
                let renamed = Stamp {
                    ctime: first.ctime,
                    ..stamp
                } == first
                    && std::os::unix::fs::MetadataExt::nlink(&meta) != access.links;
                if stamp != first && !renamed {
                    return Err(format!("{} changed while it ran", path.display()));
                }
                let digest = if access.executed {
                    store.digest_known(path, stamp)
                } else if root == roots::TARGET {
                    store.digest_placed(path, stamp, roots)
                } else {
                    store.digest_seen(path, stamp)
                }
                .map_err(|e| format!("{}: {e}", path.display()))?;
                if access.executed {
                    executed.push(path.clone());
                }
                State::File(digest)
            }
            Kind::Other if access.read => {
                return Err(format!("it read {}, not a file", path.display()));
            }
            Kind::Other => continue,
        };
        manifest.inputs.push(Input {
            root,
            path: rel.to_owned(),
            link: access
                .link
                .as_ref()
                .map(|target| roots.mark_os(target.as_os_str()).into_owned()),
            state,
        });
    }
    // Track symlinks along input paths.
    let mut walked: HashSet<PathBuf> = HashSet::new();
    let mut links = Vec::new();
    for input in &manifest.inputs {
        let Some(path) = roots.join(input.root, &input.path) else {
            continue;
        };
        for dir in path.ancestors().skip(1) {
            if dir.parent().is_none() || !walked.insert(dir.to_owned()) {
                break;
            }
            if fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink())
                && let Ok(target) = fs::read_link(dir)
            {
                links.push((dir.to_owned(), target));
            }
        }
    }
    for (dir, target) in links {
        let (root, rel) = roots.split(&dir);
        if manifest
            .inputs
            .iter()
            .any(|i| i.root == root && i.path == rel)
        {
            continue;
        }
        manifest.inputs.push(Input {
            root,
            path: rel.to_owned(),
            link: Some(roots.mark_os(target.as_os_str()).into_owned()),
            state: if dir.is_dir() {
                State::Dir(None)
            } else {
                State::Absent
            },
        });
    }
    // Track ELF dynamic loaders / script interpreters of executed binaries.
    let mut depth = 0;
    while let Some(program) = executed.pop() {
        let Some(loader) = crate::scan::interpreter(&program) else {
            continue;
        };
        let (root, rel) = roots.split(&loader);
        if manifest
            .inputs
            .iter()
            .any(|i| i.root == root && i.path == rel)
        {
            continue;
        }
        let Ok(meta) = fs::metadata(&loader) else {
            continue;
        };
        let digest = store
            .digest_known(&loader, Stamp::of(&meta))
            .map_err(|e| format!("{}: {e}", loader.display()))?;
        let link = fs::symlink_metadata(&loader)
            .ok()
            .filter(|m| m.file_type().is_symlink())
            .and_then(|_| fs::read_link(&loader).ok())
            .map(|target| roots.mark_os(target.as_os_str()).into_owned());
        manifest.inputs.push(Input {
            root,
            path: rel.to_owned(),
            link,
            state: State::File(digest),
        });
        depth += 1;
        if depth < 64 {
            executed.push(loader);
        }
    }
    // Record PATH lookups.
    let path_list = path_dirs(&req.cwd, &req.env);
    let mut names = BTreeSet::new();
    for path in report.paths.keys() {
        if let (Some(dir), Some(name)) = (path.parent(), path.file_name())
            && path_list.iter().any(|d| d == dir)
        {
            names.insert(name.to_owned());
        }
    }
    for name in names {
        let found: Option<Place> = which(&name, &path_list).map(|p| {
            let (root, rel) = roots.split(&p);
            (root, rel.to_owned())
        });
        manifest.which.push((name.as_bytes().to_vec(), found));
    }
    // Record LD_LIBRARY_PATH using loader-equivalence semantics.
    let lib_var = crate::manifest::LIBRARY_PATH;
    let lib_val = req
        .env
        .get(OsStr::from_bytes(lib_var))
        .map(|v| roots.mark_os(v).into_owned());
    manifest.env.push((lib_var.to_vec(), lib_val));

    let detail = encode_detail(0, &roots.mark(stdout), &roots.mark(stderr));
    manifest.detail = *blake3::hash(&detail).as_bytes();
    manifest.normalize();
    let mut staged = Vec::new();
    match store.commit(recipe, &manifest, &detail, ran.millis, &mut staged) {
        Ok(Some(_)) => {
            store.count("test_stored", 1);
            log(
                &test_label(&req.binary),
                format_args!(
                    "stored: ran {} ms, {} inputs",
                    ran.millis,
                    manifest.inputs.len()
                ),
            );
            Ok(())
        }
        Ok(None) => Err("larger than half the cache".into()),
        Err(e) => Err(format!("{e:#}")),
    }
}

fn run_one(
    req: &TestRequest,
    store: &mut Store,
    runner_var: &str,
    stream: bool,
) -> Result<Outcome> {
    let label = test_label(&req.binary);
    let roots = test_roots(req);
    let meta = fs::metadata(&req.binary)
        .with_context(|| format!("examining test binary {}", req.binary.display()))?;
    let binary_digest = store.digest_known(&req.binary, Stamp::of(&meta))?;
    let recipe = test_recipe(req, &roots, &binary_digest);
    if let Ok(Some(hit)) = lookup_test(req, &roots, &recipe, store) {
        if stream {
            let _ = io::stdout().write_all(&hit.stdout);
            let _ = io::stderr().write_all(&hit.stderr);
        }
        return Ok(hit);
    }
    store.count("test_miss", 1);
    let (mut ran, report) = execute_observed(req, store, runner_var, stream)?;
    if !ran.success
        && report
            .incomplete
            .as_deref()
            .is_some_and(|why| why.contains("system call listener"))
    {
        log(
            &label,
            "re-running unobserved: test requires a seccomp listener of its own",
        );
        store.count("test_uncacheable", 1);
        ran = execute_direct(req, runner_var, stream)?;
    } else if ran.success
        && let Err(why) = record_test(req, &roots, &recipe, &ran, &report, store)
    {
        store.count("test_uncacheable", 1);
        log(&label, format_args!("not stored: {why}"));
    }
    Ok(Outcome {
        code: ran.code,
        stdout: ran.stdout.unwrap_or_default(),
        stderr: ran.stderr.unwrap_or_default(),
    })
}

// ---- wrapper side (`jato __test <binary> [args...]`) ----------------------

pub fn runner(args: &[OsString]) -> Result<i32> {
    let Some((binary, rest)) = args.split_first() else {
        anyhow::bail!("missing test binary");
    };
    let direct = || {
        let mut cmd = Command::new(binary);
        cmd.args(rest);
        command::passthrough(cmd)
    };
    let Some(session) = env::var_os("JATO_TEST_SESSION") else {
        return direct();
    };
    let Ok(mut stream) = address(&session).and_then(|addr| UnixStream::connect_addr(&addr)) else {
        return direct();
    };
    let mut w = Writer(Vec::new());
    w.bytes(env::current_dir()?.as_os_str().as_bytes());
    w.bytes(binary.as_bytes());
    w.number(rest.len() as u64);
    for arg in rest {
        w.bytes(arg.as_bytes());
    }
    let vars: Vec<_> = own_environment().collect();
    w.number(vars.len() as u64);
    for (k, v) in &vars {
        w.bytes(k.as_bytes());
        w.bytes(v.as_bytes());
    }
    if wire::send(&mut stream, &w.0).is_err() {
        return direct();
    }
    let Ok(reply) = wire::receive(&mut stream) else {
        return direct();
    };
    let mut r = Reader(&reply);
    match r.byte()? {
        QUEUED => Ok(0),
        DONE => {
            let code = r.number()? as i32;
            io::stdout().write_all(r.bytes()?)?;
            io::stderr().write_all(r.bytes()?)?;
            Ok(code)
        }
        RUN_SYNC => {
            let runner_var = String::from_utf8_lossy(r.bytes()?).into_owned();
            let req = TestRequest {
                cwd: env::current_dir()?,
                binary: PathBuf::from(binary),
                args: rest.to_vec(),
                env: vars.into_iter().collect(),
            };
            let mut store = Store::open()?;
            let outcome = run_one(&req, &mut store, &runner_var, true)?;
            store.flush();
            Ok(outcome.code)
        }
        _ => direct(),
    }
}

// ---- supervisor side (`jato test ...`) ------------------------------------

struct ParallelResult {
    binary: PathBuf,
    outcome: Option<Outcome>,
}

struct Pool {
    queue: Mutex<PoolState>,
    ready: Condvar,
    done: Condvar,
    stores: Mutex<Vec<Store>>,
    runner_var: String,
    fail_fast: bool,
    aborted: AtomicBool,
}

struct PoolState {
    tasks: VecDeque<(usize, TestRequest)>,
    results: HashMap<usize, ParallelResult>,
    closed: bool,
}

impl Pool {
    fn open_store(&self) -> Result<Store> {
        if let Some(store) = self.stores.lock().unwrap_or_else(|e| e.into_inner()).pop() {
            return Ok(store);
        }
        Store::open()
    }

    fn return_store(&self, mut store: Store) {
        store.flush();
        self.stores
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(store);
    }

    fn worker(self: Arc<Self>) {
        loop {
            let task = {
                let mut state = self.queue.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if let Some(task) = state.tasks.pop_front() {
                        break Some(task);
                    }
                    if state.closed {
                        break None;
                    }
                    state = self.ready.wait(state).unwrap_or_else(|e| e.into_inner());
                }
            };
            let Some((seq, req)) = task else {
                return;
            };
            let outcome = if self.fail_fast && self.aborted.load(Ordering::Acquire) {
                None
            } else {
                match self.open_store() {
                    Ok(mut store) => {
                        let res = run_one(&req, &mut store, &self.runner_var, false).ok();
                        self.return_store(store);
                        res
                    }
                    Err(_) => None,
                }
            };
            if let Some(out) = &outcome
                && out.code != 0
                && self.fail_fast
            {
                self.aborted.store(true, Ordering::Release);
            }
            let mut state = self.queue.lock().unwrap_or_else(|e| e.into_inner());
            state.results.insert(
                seq,
                ParallelResult {
                    binary: req.binary,
                    outcome,
                },
            );
            self.done.notify_all();
        }
    }
}

fn read_request(stream: &mut UnixStream) -> Result<TestRequest> {
    let msg = wire::receive(stream)?;
    let mut r = Reader(&msg);
    let cwd = PathBuf::from(os(r.bytes()?));
    let binary = PathBuf::from(os(r.bytes()?));
    let mut args = Vec::new();
    for _ in 0..r.count()? {
        args.push(os(r.bytes()?));
    }
    let mut env = Env::new();
    for _ in 0..r.count()? {
        let k = os(r.bytes()?);
        env.insert(k, os(r.bytes()?));
    }
    Ok(TestRequest {
        cwd,
        binary,
        args,
        env,
    })
}

/// Strip ANSI escapes just enough to classify Cargo's stderr lines.
fn plain_trimmed(line: &[u8]) -> String {
    let mut out = Vec::with_capacity(line.len());
    let mut i = 0;
    while i < line.len() {
        if line[i] == 0x1b && line.get(i + 1) == Some(&b'[') {
            i += 2;
            while i < line.len() && !line[i].is_ascii_alphabetic() {
                i += 1;
            }
            i += 1;
        } else {
            out.push(line[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).trim_start().to_owned()
}

/// Run `cargo test` with observed test caching and (when appropriate)
/// parallel workspace test binary execution.
pub fn run(mut cmd: Command, args: &[OsString]) -> Result<i32> {
    command::started();
    if env::var_os("JATO_DISABLE").is_some_and(|v| v == "1") || has_list_flag(args) {
        return command::passthrough(cmd);
    }
    let cwd = env::current_dir()?;
    let project_cfg = load_project_test_config(args, &cwd);
    let concurrency = resolve_test_concurrency(
        args,
        project_cfg,
        env::var_os("JATO_PARALLEL_TESTS").as_deref(),
        env::var_os("JATO_TEST_THREADS").as_deref(),
        env::var_os("RUST_TEST_THREADS").as_deref(),
        default_test_workers(),
    );
    if let Some(threads) = concurrency.rust_test_threads {
        cmd.env("RUST_TEST_THREADS", threads.to_string());
    }
    log(
        "test",
        format_args!(
            "concurrency: binaries={}, test_threads={}",
            concurrency.parallel_workers.unwrap_or(1),
            concurrency
                .rust_test_threads
                .map_or_else(|| "default".to_owned(), |n| n.to_string()),
        ),
    );
    let end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let is_doc = args[..end].iter().any(|a| a == "--doc");
    if is_doc || env::var_os("JATO_CACHE_TESTS").is_some_and(|v| v == "0") || !observe::available()
    {
        return command::passthrough(cmd);
    }
    let exe = env::current_exe()?;
    if exe
        .as_os_str()
        .as_bytes()
        .iter()
        .any(u8::is_ascii_whitespace)
    {
        return command::passthrough(cmd);
    }
    let Some(triple) = target_triple(args) else {
        return command::passthrough(cmd);
    };
    let runner_var = runner_env_name(&triple);
    if has_custom_runner(&triple, &runner_var, &cwd) {
        return command::passthrough(cmd);
    }

    let fail_fast = !args[..end].iter().any(|a| a == "--no-fail-fast");
    let socket = format!(
        "jato-test-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let listener = UnixListener::bind_addr(&address(OsStr::new(&socket))?)?;
    let mut runner_val = exe.as_os_str().to_owned();
    runner_val.push(" __test");
    cmd.env(&runner_var, runner_val)
        .env("JATO_TEST_SESSION", &socket);

    let Some(workers) = concurrency.parallel_workers else {
        // Synchronous mode: Cargo owns stdout/stderr directly; supervisor
        // serves instant cache hits and hands misses back for live streaming.
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let rv = runner_var.clone();
        let accept = std::thread::spawn(move || {
            let mut store = Store::open().ok();
            for mut stream in listener.incoming().flatten() {
                if stopping.load(Ordering::Acquire) {
                    break;
                }
                if !wire::same_user(&stream) {
                    continue;
                }
                let Ok(req) = read_request(&mut stream) else {
                    continue;
                };
                if let Some(store) = store.as_mut() {
                    let roots = test_roots(&req);
                    if let Ok(meta) = fs::metadata(&req.binary)
                        && let Ok(digest) = store.digest_known(&req.binary, Stamp::of(&meta))
                    {
                        let recipe = test_recipe(&req, &roots, &digest);
                        if let Ok(Some(hit)) = lookup_test(&req, &roots, &recipe, store) {
                            store.flush();
                            let mut w = Writer(vec![DONE]);
                            w.number(hit.code as u32 as u64);
                            w.bytes(&hit.stdout);
                            w.bytes(&hit.stderr);
                            let _ = wire::send(&mut stream, &w.0);
                            continue;
                        }
                    }
                }
                let mut w = Writer(vec![RUN_SYNC]);
                w.bytes(rv.as_bytes());
                let _ = wire::send(&mut stream, &w.0);
            }
        });
        let status = cmd.status().context("running cargo test")?;
        stop.store(true, Ordering::Release);
        let _ = UnixStream::connect_addr(&address(OsStr::new(&socket))?);
        let _ = accept.join();
        return Ok(exit_code(status));
    };

    // Parallel mode: Cargo dispatches every test binary rapidly via `__test`
    // while our worker pool executes cache hits & observed misses across cores.

    let pool = Arc::new(Pool {
        queue: Mutex::new(PoolState {
            tasks: VecDeque::new(),
            results: HashMap::new(),
            closed: false,
        }),
        ready: Condvar::new(),
        done: Condvar::new(),
        stores: Mutex::new(Vec::new()),
        runner_var,
        fail_fast,
        aborted: AtomicBool::new(false),
    });
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let p = pool.clone();
        handles.push(std::thread::spawn(move || p.worker()));
    }

    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let pool_accept = pool.clone();
    let dispatched = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let disp_accept = dispatched.clone();
    let accept = std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            if stopping.load(Ordering::Acquire) {
                break;
            }
            if !wire::same_user(&stream) {
                continue;
            }
            let Ok(req) = read_request(&mut stream) else {
                let _ = wire::send(&mut stream, &[DECLINE]);
                continue;
            };
            let seq = disp_accept.fetch_add(1, Ordering::AcqRel);
            {
                let mut state = pool_accept.queue.lock().unwrap_or_else(|e| e.into_inner());
                state.tasks.push_back((seq, req));
            }
            pool_accept.ready.notify_one();
            let _ = wire::send(&mut stream, &[QUEUED]);
        }
    });

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().context("running cargo test")?;
    let out_pipe = child.stdout.take().unwrap();
    let err_pipe = child.stderr.take().unwrap();
    let read_out = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut r = out_pipe;
        let _ = r.read_to_end(&mut buf);
        buf
    });
    let read_err = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut r = err_pipe;
        let _ = r.read_to_end(&mut buf);
        buf
    });
    let status = child.wait().context("waiting for cargo test")?;
    let cargo_out = read_out.join().unwrap_or_default();
    let cargo_err = read_err.join().unwrap_or_default();

    stop.store(true, Ordering::Release);
    let _ = UnixStream::connect_addr(&address(OsStr::new(&socket))?);
    let _ = accept.join();
    {
        let mut state = pool.queue.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
    }
    pool.ready.notify_all();

    let total = dispatched.load(Ordering::Acquire);
    if total == 0 {
        for h in handles {
            let _ = h.join();
        }
        io::stderr().write_all(&cargo_err)?;
        io::stdout().write_all(&cargo_out)?;
        return Ok(exit_code(status));
    }

    // Partition Cargo's stderr into preamble, per-binary `Running ...` headers,
    // and post-test lines (such as `Doc-tests ...`).
    let mut before = Vec::new();
    let mut headers = Vec::new();
    let mut after = Vec::new();
    let mut seen_running = false;
    let mut in_after = false;
    for line in cargo_err.split_inclusive(|b| *b == b'\n') {
        let trimmed = plain_trimmed(line);
        if trimmed.starts_with("Running ") && !in_after {
            seen_running = true;
            headers.push(line.to_vec());
        } else if !seen_running {
            before.extend_from_slice(line);
        } else {
            in_after = true;
            after.extend_from_slice(line);
        }
    }
    io::stderr().write_all(&before)?;
    io::stderr().flush()?;

    // Stream each test binary's output in exact deterministic order as soon
    // as its parallel worker completes.
    let mut failure_code: Option<i32> = None;
    for seq in 0..total {
        let result = {
            let mut state = pool.queue.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(res) = state.results.remove(&seq) {
                    break res;
                }
                state = pool.done.wait(state).unwrap_or_else(|e| e.into_inner());
            }
        };
        let Some(outcome) = result.outcome else {
            // Skipped due to fail-fast after an earlier test failed.
            continue;
        };
        if let Some(header) = headers.get(seq) {
            io::stderr().write_all(header)?;
            io::stderr().flush()?;
        }
        io::stdout().write_all(&outcome.stdout)?;
        io::stdout().flush()?;
        io::stderr().write_all(&outcome.stderr)?;
        io::stderr().flush()?;
        if outcome.code != 0 && failure_code.is_none() {
            failure_code = Some(outcome.code);
            let _ = writeln!(
                io::stderr(),
                "error: test failed ({})",
                result.binary.display()
            );
        }
    }
    for h in handles {
        let _ = h.join();
    }
    // Flush pooled stores so stats are visible immediately.
    pool.stores
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();

    if failure_code.is_none() || !fail_fast {
        io::stderr().write_all(&after)?;
        io::stdout().write_all(&cargo_out)?;
    }
    if let Some(code) = failure_code {
        return Ok(code);
    }
    Ok(exit_code(status))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<OsString> {
        s.split_whitespace().map(OsString::from).collect()
    }

    #[test]
    fn parses_jato_test_metadata_in_all_toml_forms() {
        let standard = r#"
[workspace]
members = ["a", "b"]

[workspace.metadata.jato]
parallel-tests = 4 # keep Postgres connections bounded
test-threads = 2
"#;
        let (ws, pkg) = parse_manifest_test_config(standard);
        assert_eq!(
            ws,
            ProjectTestConfig {
                parallel_tests: Some(4),
                test_threads: Some(2),
            }
        );
        assert_eq!(pkg, ProjectTestConfig::default());

        // Snake-case aliases, inline tables, dotted keys, and false (= 0 / serial).
        let inline = r#"
[package]
name = "demo"
description = """
[workspace.metadata.jato]
parallel-tests = 99
"""

[package.metadata]
jato = { parallel_tests = false, test_threads = 3 }

[[bin]]
name = "ignored"
parallel-tests = 99
"#;
        let (ws, pkg) = parse_manifest_test_config(inline);
        assert_eq!(ws, ProjectTestConfig::default());
        assert_eq!(
            pkg,
            ProjectTestConfig {
                parallel_tests: Some(0),
                test_threads: Some(3),
            }
        );

        let dotted = r#"
[workspace.metadata]
jato.parallel-tests = 1_6
jato.test-threads = 4
"#;
        let (ws, _) = parse_manifest_test_config(dotted);
        assert_eq!(
            ws,
            ProjectTestConfig {
                parallel_tests: Some(16),
                test_threads: Some(4),
            }
        );
    }

    #[test]
    fn loads_workspace_and_member_metadata_with_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("crates/a")).unwrap();
        fs::create_dir_all(root.join("crates/b")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            r#"
[workspace]
members = ["crates/a", "crates/b"]

[workspace.metadata.jato]
parallel-tests = 4
test-threads = 3
"#,
        )
        .unwrap();
        fs::write(
            root.join("crates/a/Cargo.toml"),
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(
            root.join("crates/b/Cargo.toml"),
            "[package]\nname = \"b\"\nversion = \"0.1.0\"\n\n[package.metadata.jato]\ntest-threads = 1\n",
        )
        .unwrap();

        // Workspace root uses [workspace.metadata.jato].
        assert_eq!(
            load_project_test_config(&words("test --workspace"), root),
            ProjectTestConfig {
                parallel_tests: Some(4),
                test_threads: Some(3),
            }
        );
        // Member without override inherits workspace settings.
        assert_eq!(
            load_project_test_config(&words("test"), &root.join("crates/a")),
            ProjectTestConfig {
                parallel_tests: Some(4),
                test_threads: Some(3),
            }
        );
        // Member with [package.metadata.jato] overrides only its specified key.
        assert_eq!(
            load_project_test_config(&words("test"), &root.join("crates/b")),
            ProjectTestConfig {
                parallel_tests: Some(4),
                test_threads: Some(1),
            }
        );
        // Explicit --manifest-path resolves to that member + its enclosing workspace.
        let mf = root.join("crates/b/Cargo.toml");
        let args = vec![
            OsString::from("test"),
            OsString::from("--manifest-path"),
            mf.into_os_string(),
        ];
        assert_eq!(
            load_project_test_config(&args, dir.path()),
            ProjectTestConfig {
                parallel_tests: Some(4),
                test_threads: Some(1),
            }
        );
    }

    #[test]
    fn resolves_test_concurrency_precedence_and_caps() {
        let project = ProjectTestConfig {
            parallel_tests: Some(4),
            test_threads: Some(3),
        };
        let os = OsStr::new;

        // Default with no config preserves full machine workers.
        assert_eq!(
            resolve_test_concurrency(
                &words("test --workspace"),
                ProjectTestConfig::default(),
                None,
                None,
                None,
                32,
            ),
            TestConcurrency {
                parallel_workers: Some(32),
                rust_test_threads: None,
            }
        );

        // Project cap bounds workers on larger machines and respects smaller machines.
        assert_eq!(
            resolve_test_concurrency(&words("test"), project, None, None, None, 32),
            TestConcurrency {
                parallel_workers: Some(4),
                rust_test_threads: Some(3),
            }
        );
        assert_eq!(
            resolve_test_concurrency(&words("test"), project, None, None, None, 2),
            TestConcurrency {
                parallel_workers: Some(2),
                rust_test_threads: Some(3),
            }
        );

        // Env vars override Cargo.toml in both directions (including 0/1 -> serial and 0 -> unset threads).
        assert_eq!(
            resolve_test_concurrency(
                &words("test"),
                project,
                Some(os("1")),
                Some(os("0")),
                None,
                32
            ),
            TestConcurrency {
                parallel_workers: None,
                rust_test_threads: None,
            }
        );
        assert_eq!(
            resolve_test_concurrency(
                &words("test"),
                project,
                Some(os("0")),
                Some(os("2")),
                None,
                32
            ),
            TestConcurrency {
                parallel_workers: None,
                rust_test_threads: Some(2),
            }
        );
        assert_eq!(
            resolve_test_concurrency(
                &words("test"),
                project,
                Some(os("8")),
                Some(os("2")),
                Some(os("6")),
                32,
            ),
            TestConcurrency {
                parallel_workers: Some(8),
                rust_test_threads: Some(2),
            }
        );

        // RUST_TEST_THREADS overrides Cargo.toml when JATO_TEST_THREADS is unset.
        assert_eq!(
            resolve_test_concurrency(&words("test"), project, None, None, Some(os("6")), 32),
            TestConcurrency {
                parallel_workers: Some(4),
                rust_test_threads: None,
            }
        );

        // --nocapture always runs synchronously; --test-threads on CLI skips injecting RUST_TEST_THREADS.
        assert_eq!(
            resolve_test_concurrency(
                &words("test -- --nocapture"),
                project,
                Some(os("8")),
                None,
                None,
                32,
            ),
            TestConcurrency {
                parallel_workers: None,
                rust_test_threads: Some(3),
            }
        );
        assert_eq!(
            resolve_test_concurrency(
                &words("test -- --test-threads=1"),
                project,
                None,
                None,
                None,
                32,
            ),
            TestConcurrency {
                parallel_workers: None,
                rust_test_threads: None,
            }
        );
        assert_eq!(
            resolve_test_concurrency(
                &words("test -- --test-threads=1"),
                project,
                Some(os("4")),
                None,
                None,
                32,
            ),
            TestConcurrency {
                parallel_workers: Some(4),
                rust_test_threads: None,
            }
        );
    }
}
