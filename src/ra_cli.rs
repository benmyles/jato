//! Instant CLI semantic queries backed by the shared `jato rust-analyzer` daemon.
//!
//! Exposes `jato diag`, `jato def`, `jato refs`, `jato hover`, and `jato symbols`
//! over the warm Salsa DB held by `jato rust-analyzer`, giving humans and coding
//! agents millisecond-latency typecheck diagnostics and code navigation without
//! spawning Cargo.

use super::{
    ClientEnvOverrides, Handshake, connect_or_start_daemon_until, encode_lsp_message, path_to_uri,
    read_lsp_message, uri_to_path,
};
use crate::cas::cache_dir;
use crate::roots;
use crate::wire;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 86_400;

const DIAG_HELP: &str = "\
Usage: jato diag [OPTIONS] [FILE...]

Instant file or workspace diagnostics from the warm `jato rust-analyzer` daemon.
Exits with status 0 when no errors are found, 1 when errors are present.

Options:
  --workspace, --all          Check all `.rs` files in the workspace
  --check, --rustc            Run full compiler diagnostics via `jato check --message-format=json`
                              (catches borrow-checker, lifetime, macro, and lint errors)
  --errors-only               Only report error-severity diagnostics
  --severity LEVEL            Minimum severity: error, warning (default), info, hint
  --manifest-path PATH        Locate workspace from PATH instead of current directory
  --timeout SECS              Max seconds to wait (1..=86400; default: 120)
  --json                      Emit structured JSON output
  -h, --help                  Show this help

When no FILE is given and neither --workspace nor --check is passed, checks
modified/untracked `.rs` files from `git status`, or the target crate's `.rs`
files when the working tree is clean.
";

const DEF_HELP: &str = "\
Usage: jato def [OPTIONS] <FILE:LINE:COL | FILE LINE COL>

Go to definition using the warm `jato rust-analyzer` daemon (1-based line and column).

Options:
  --type                      Go to type definition instead
  --impl                      Go to implementations instead
  --timeout SECS              Max seconds to wait (1..=86400; default: 120)
  --json                      Emit structured JSON output
  -h, --help                  Show this help
";

const REFS_HELP: &str = "\
Usage: jato refs [OPTIONS] <FILE:LINE:COL | FILE LINE COL>

Find references using the warm `jato rust-analyzer` daemon (1-based line and column).

Options:
  --no-decl                   Exclude the declaration site itself
  --limit N                   Max results to print (0 = unlimited; default: 200 text, 0 --json)
  --timeout SECS              Max seconds to wait (1..=86400; default: 120)
  --json                      Emit structured JSON output
  -h, --help                  Show this help
";

const HOVER_HELP: &str = "\
Usage: jato hover [OPTIONS] <FILE:LINE:COL | FILE LINE COL>

Show type signature and documentation at a position (1-based line and column).

Options:
  --timeout SECS              Max seconds to wait (1..=86400; default: 120)
  --json                      Emit structured JSON output
  -h, --help                  Show this help
";

const SYMBOLS_HELP: &str = "\
Usage: jato symbols [OPTIONS] [FILE | --query QUERY]

List symbols in a Rust source file (`textDocument/documentSymbol`) or search
across the workspace (`workspace/symbol`).

Options:
  -q, --query QUERY           Search workspace symbols matching QUERY
  -w, --workspace QUERY       Alias for --query QUERY
  --manifest-path PATH        Locate workspace from PATH for workspace symbol search
  --limit N                   Max workspace symbols to print (0 = unlimited; default: 200 text, 0 --json)
  --timeout SECS              Max seconds to wait (1..=86400; default: 120)
  --json                      Emit structured JSON output
  -h, --help                  Show this help
";

fn parse_timeout(raw: &str) -> Result<u64> {
    let secs: u64 = raw
        .parse()
        .ok()
        .filter(|&v| (1..=MAX_TIMEOUT_SECS).contains(&v))
        .with_context(|| {
            format!("--timeout expects an integer between 1 and {MAX_TIMEOUT_SECS}")
        })?;
    Ok(secs)
}

fn make_deadline(started: Instant, timeout_secs: u64) -> Instant {
    let dur = Duration::from_secs(timeout_secs.min(MAX_TIMEOUT_SECS));
    started
        .checked_add(dur)
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(DEFAULT_TIMEOUT_SECS))
}

fn next_flag_value<'a>(args: &'a [OsString], i: &mut usize, flag: &str) -> Result<&'a OsStr> {
    if *i + 1 >= args.len() {
        bail!("missing value for `{flag}`");
    }
    *i += 1;
    Ok(args[*i].as_os_str())
}

// ---- thin LSP client over the shared daemon socket -------------------------

struct CliLspSession {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    deadline: Instant,
    timeout_secs: u64,
    opened: HashMap<String, String>,
    closed: bool,
}

impl CliLspSession {
    fn connect(workspace_root: &Path, deadline: Instant, timeout_secs: u64) -> Result<Self> {
        let cache = cache_dir()?;
        let mut overrides = ClientEnvOverrides::from_env();
        // CLI queries always want the backend ready immediately.
        overrides.lazy = Some(false);
        let handshake = Handshake::Lsp {
            cwd: workspace_root.to_path_buf(),
            pid: std::process::id(),
            overrides,
            cli: true,
        };
        let hs_bytes = serde_json::to_vec(&handshake)?;

        let mut stream = None;
        let mut last_err = None;
        for _ in 0..10 {
            let Some(rem) = deadline.checked_duration_since(Instant::now()) else {
                bail!("timed out after {timeout_secs} s connecting to jato rust-analyzer daemon");
            };
            match connect_or_start_daemon_until(&cache, deadline) {
                Ok(mut s) => {
                    let io_t = rem
                        .min(Duration::from_secs(5))
                        .max(Duration::from_millis(50));
                    let _ = s.set_read_timeout(Some(io_t));
                    let _ = s.set_write_timeout(Some(io_t));
                    if wire::send(&mut s, &hs_bytes).is_ok() && wire::receive(&mut s).is_ok() {
                        stream = Some(s);
                        break;
                    }
                    last_err = Some(anyhow::anyhow!("daemon closed before handshake ack"));
                }
                Err(e) => last_err = Some(e),
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let writer = match stream {
            Some(s) => s,
            None => return Err(last_err.unwrap_or_else(|| anyhow::anyhow!("connect failed"))),
        };
        let reader_stream = writer.try_clone()?;
        let mut sess = Self {
            reader: BufReader::new(reader_stream),
            writer,
            next_id: 1,
            deadline,
            timeout_secs,
            opened: HashMap::new(),
            closed: false,
        };

        let root_uri = path_to_uri(workspace_root);
        let name = workspace_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("workspace");
        let _init = sess.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "workspaceFolders": [{ "uri": root_uri, "name": name }],
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
                        "documentSymbol": { "hierarchicalDocumentSymbolSupport": true }
                    }
                }
            }),
        )?;
        sess.notify("initialized", json!({}))?;
        Ok(sess)
    }

    fn send_raw(&mut self, msg: &Value) -> Result<()> {
        let bytes = encode_lsp_message(msg);
        let rem = self
            .deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_secs(10))
            .max(Duration::from_millis(50));
        let _ = self.writer.set_write_timeout(Some(rem));
        self.writer
            .write_all(&bytes)
            .context("writing LSP frame to jato rust-analyzer daemon")?;
        self.writer.flush()?;
        Ok(())
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send_raw(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        }))
    }

    fn open_file(&mut self, path: &Path) -> Result<(String, String)> {
        let canon = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let uri = path_to_uri(&canon);
        if let Some(text) = self.opened.get(&uri) {
            return Ok((uri, text.clone()));
        }
        let text =
            fs::read_to_string(&canon).with_context(|| format!("reading {}", canon.display()))?;
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "rust",
                    "version": 1,
                    "text": text
                }
            }),
        )?;
        self.opened.insert(uri.clone(), text.clone());
        Ok((uri, text))
    }

    fn close_file(&mut self, uri: &str) {
        if self.opened.remove(uri).is_some() {
            let _ = self.notify(
                "textDocument/didClose",
                json!({ "textDocument": { "uri": uri } }),
            );
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send_raw(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))?;

        loop {
            let Some(remaining) = self.deadline.checked_duration_since(Instant::now()) else {
                bail!(
                    "timed out after {} s waiting for `{method}` from jato rust-analyzer (run `jato rust-analyzer status` to inspect)",
                    self.timeout_secs
                );
            };
            let _ = self
                .reader
                .get_ref()
                .set_read_timeout(Some(remaining.max(Duration::from_millis(50))));
            let msg = match read_lsp_message(&mut self.reader) {
                Ok(Some(m)) => m,
                Ok(None) => bail!("jato rust-analyzer daemon closed connection during `{method}`"),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    if Instant::now() >= self.deadline {
                        bail!(
                            "timed out after {} s waiting for `{method}` from jato rust-analyzer (run `jato rust-analyzer status` to inspect)",
                            self.timeout_secs
                        );
                    }
                    continue;
                }
                Err(e) => return Err(e).context("reading LSP response from daemon"),
            };
            if msg.get("id").and_then(Value::as_u64) == Some(id) && msg.get("method").is_none() {
                if let Some(err) = msg.get("error") {
                    let code = err.get("code").and_then(Value::as_i64).unwrap_or(-1);
                    let m = err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown LSP error");
                    bail!("rust-analyzer `{method}` failed ({code}): {m}");
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
            // Answer any server-to-client request cleanly so nothing stalls.
            if let Some(srv_id) = msg.get("id")
                && msg.get("method").is_some()
            {
                let _ = self.send_raw(&json!({
                    "jsonrpc": "2.0",
                    "id": srv_id,
                    "result": null
                }));
            }
        }
    }

    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        let uris: Vec<String> = self.opened.drain().map(|(u, _)| u).collect();
        for uri in uris {
            let _ = self.notify(
                "textDocument/didClose",
                json!({ "textDocument": { "uri": uri } }),
            );
        }
        let id = self.next_id;
        self.next_id += 1;
        let _ = self.send_raw(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "shutdown",
            "params": null
        }));
        let _ = self.notify("exit", Value::Null);
    }
}

impl Drop for CliLspSession {
    fn drop(&mut self) {
        self.close();
    }
}

// ---- location, path & UTF-16 helpers ---------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedLocation {
    path: PathBuf,
    /// 1-based line number.
    line: u32,
    /// 1-based Unicode character column.
    col: u32,
}

fn parse_pos_u32(s: &str) -> Option<u32> {
    let n: u32 = s.parse().ok()?;
    (n >= 1).then_some(n)
}

fn parse_location_args(positional: &[String]) -> Result<ParsedLocation> {
    match positional {
        [] => bail!("expected <FILE:LINE:COL> or <FILE> <LINE> <COL>"),
        [spec] => parse_location_spec(spec),
        [file, line] => {
            let line = parse_pos_u32(line)
                .with_context(|| format!("invalid 1-based line number `{line}`"))?;
            Ok(ParsedLocation {
                path: PathBuf::from(file),
                line,
                col: 1,
            })
        }
        [file, line, col] => {
            let line = parse_pos_u32(line)
                .with_context(|| format!("invalid 1-based line number `{line}`"))?;
            let col = parse_pos_u32(col)
                .with_context(|| format!("invalid 1-based column number `{col}`"))?;
            Ok(ParsedLocation {
                path: PathBuf::from(file),
                line,
                col,
            })
        }
        _ => {
            bail!("too many positional arguments (expected <FILE:LINE:COL> or <FILE> <LINE> <COL>)")
        }
    }
}

fn parse_location_spec(spec: &str) -> Result<ParsedLocation> {
    if let Some((prefix, last)) = spec.rsplit_once(':')
        && let Some(last_num) = parse_pos_u32(last)
    {
        if let Some((file, line_str)) = prefix.rsplit_once(':')
            && !file.is_empty()
            && !line_str.is_empty()
            && line_str.bytes().all(|b| b.is_ascii_digit())
        {
            let line_num = parse_pos_u32(line_str)
                .with_context(|| format!("invalid 1-based line number `{line_str}` in `{spec}`"))?;
            return Ok(ParsedLocation {
                path: PathBuf::from(file),
                line: line_num,
                col: last_num,
            });
        }
        if !prefix.is_empty() {
            return Ok(ParsedLocation {
                path: PathBuf::from(prefix),
                line: last_num,
                col: 1,
            });
        }
    }
    bail!("invalid location `{spec}` (expected FILE:LINE:COL, e.g. src/lib.rs:42:9)")
}

/// Convert a 1-based `(line, char_col)` in `text` to LSP 0-based `(line, utf16_col)`.
fn char_pos_to_lsp(text: &str, line_1: u32, col_1: u32) -> (u32, u32) {
    let target_line = line_1.saturating_sub(1);
    let target_char = col_1.saturating_sub(1) as usize;
    let Some(raw_line) = text.split('\n').nth(target_line as usize) else {
        return (target_line, target_char as u32);
    };
    let line_str = raw_line.strip_suffix('\r').unwrap_or(raw_line);
    let mut utf16 = 0u32;
    for (idx, ch) in line_str.chars().enumerate() {
        if idx >= target_char {
            break;
        }
        utf16 += ch.len_utf16() as u32;
    }
    (target_line, utf16)
}

/// Convert an LSP 0-based UTF-16 column on `line_str` to a 1-based Unicode character column.
fn lsp_utf16_to_col_1(line_str: &str, utf16_col: u32) -> u32 {
    let mut utf16 = 0u32;
    let mut chars = 0u32;
    for ch in line_str.chars() {
        if utf16 >= utf16_col {
            break;
        }
        utf16 += ch.len_utf16() as u32;
        chars += 1;
    }
    chars + 1
}

struct LineCache {
    files: HashMap<PathBuf, Vec<String>>,
}

impl LineCache {
    fn new() -> Self {
        Self {
            files: HashMap::new(),
        }
    }

    fn line(&mut self, path: &Path, line_0: u32) -> Option<&str> {
        if !self.files.contains_key(path) {
            let lines = fs::read_to_string(path)
                .ok()
                .map(|s| {
                    s.split('\n')
                        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_owned())
                        .collect()
                })
                .unwrap_or_default();
            self.files.insert(path.to_path_buf(), lines);
        }
        self.files
            .get(path)
            .and_then(|v| v.get(line_0 as usize))
            .map(String::as_str)
    }

    fn col_1(&mut self, path: &Path, line_0: u32, utf16_col: u32) -> u32 {
        match self.line(path, line_0) {
            Some(l) => lsp_utf16_to_col_1(l, utf16_col),
            None => utf16_col + 1,
        }
    }

    fn preview(&mut self, path: &Path, line_0: u32) -> Option<String> {
        self.line(path, line_0).map(|l| l.trim().to_owned())
    }
}

fn display_path(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn resolve_manifest_path(cwd: &Path, raw: &Path) -> Result<PathBuf> {
    let abs = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };
    if !abs.is_file() {
        bail!("manifest path not found: {}", raw.display());
    }
    Ok(fs::canonicalize(&abs).unwrap_or(abs))
}

fn resolve_workspace_root(hint: Option<&Path>) -> Result<PathBuf> {
    let start = match hint {
        Some(p) => {
            let abs = if p.is_absolute() {
                p.to_path_buf()
            } else {
                env::current_dir()?.join(p)
            };
            let canon = fs::canonicalize(&abs).unwrap_or(abs);
            if canon.is_file() {
                canon.parent().unwrap_or(&canon).to_path_buf()
            } else {
                canon
            }
        }
        None => {
            let cwd = env::current_dir()?;
            fs::canonicalize(&cwd).unwrap_or(cwd)
        }
    };
    Ok(roots::workspace_above(&start)
        .and_then(|p| fs::canonicalize(&p).ok().or(Some(p)))
        .unwrap_or(start))
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if name_str == "target" || name_str == ".git" || name_str.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            collect_rs_files(&path, out);
        } else if ft.is_file() && path.extension().is_some_and(|e| e == "rs") {
            out.push(fs::canonicalize(&path).unwrap_or(path));
        }
    }
}

fn parse_git_porcelain_z(stdout: &[u8], repo_root: &Path, workspace_root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut iter = stdout.split(|&b| b == 0);
    while let Some(entry) = iter.next() {
        if entry.len() < 4 {
            continue;
        }
        let status = &entry[..2];
        // Renames/copies always emit a second NUL-terminated old path; consume
        // it BEFORE filtering out deleted entries (`RD`/`CD`).
        if matches!(status[0], b'R' | b'C') || matches!(status[1], b'R' | b'C') {
            let _ = iter.next();
        }
        if status[0] == b'D' || status[1] == b'D' {
            continue;
        }
        let rel = Path::new(OsStr::from_bytes(&entry[3..]));
        let full = repo_root.join(rel);
        if full.extension().is_some_and(|e| e == "rs")
            && full.is_file()
            && let Ok(canon) = fs::canonicalize(&full)
            && canon.starts_with(workspace_root)
        {
            files.push(canon);
        }
    }
    files.sort();
    files.dedup();
    files
}

fn git_modified_rs_files(workspace_root: &Path) -> Option<Vec<PathBuf>> {
    let out = Command::new("git")
        .args(["status", "--porcelain", "-z", "--untracked-files=all"])
        .current_dir(workspace_root)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // Find git top-level because `git status --porcelain` paths are relative to repo root.
    let top_out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(workspace_root)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let repo_root = if top_out.status.success() {
        let raw = top_out
            .stdout
            .strip_suffix(b"\n")
            .unwrap_or(&top_out.stdout);
        let p = PathBuf::from(OsStr::from_bytes(raw));
        fs::canonicalize(&p).unwrap_or(p)
    } else {
        workspace_root.to_path_buf()
    };
    Some(parse_git_porcelain_z(
        &out.stdout,
        &repo_root,
        workspace_root,
    ))
}

fn nearest_crate_root(from: &Path) -> PathBuf {
    for dir in from.ancestors() {
        if dir.join("Cargo.toml").is_file() {
            return dir.to_path_buf();
        }
    }
    from.to_path_buf()
}

// ---- structured output types -----------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
struct DiagItem {
    file: String,
    line: u32,
    col: u32,
    end_line: u32,
    end_col: u32,
    severity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    message: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct LocationItem {
    file: String,
    line: u32,
    col: u32,
    end_line: u32,
    end_col: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    preview: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SymbolItem {
    name: String,
    kind: String,
    file: String,
    line: u32,
    col: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    container: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    children: Vec<SymbolItem>,
}

fn severity_rank(name: &str) -> Result<u8> {
    match name.to_ascii_lowercase().as_str() {
        "error" | "err" => Ok(1),
        "warning" | "warn" => Ok(2),
        "info" | "information" => Ok(3),
        "hint" => Ok(4),
        other => bail!("invalid --severity `{other}` (expected error, warning, info, or hint)"),
    }
}

fn lsp_severity_name(code: u64) -> &'static str {
    match code {
        1 => "error",
        2 => "warning",
        3 => "info",
        _ => "hint",
    }
}

fn lsp_symbol_kind(kind: u64) -> &'static str {
    match kind {
        1 => "file",
        2 => "mod",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "trait",
        12 => "fn",
        13 => "var",
        14 => "const",
        15 => "string",
        16 => "number",
        17 => "bool",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "variant",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type_param",
        _ => "symbol",
    }
}

fn parse_lsp_locations(result: &Value, base: &Path, lines: &mut LineCache) -> Vec<LocationItem> {
    let items: Vec<&Value> = match result {
        Value::Array(arr) => arr.iter().collect(),
        Value::Object(_) => vec![result],
        _ => Vec::new(),
    };
    let mut out = Vec::new();
    for item in items {
        let (uri, range) = if let Some(target_uri) = item.get("targetUri").and_then(Value::as_str) {
            let r = item
                .get("targetSelectionRange")
                .or_else(|| item.get("targetRange"));
            (target_uri, r)
        } else if let Some(u) = item.get("uri").and_then(Value::as_str) {
            (u, item.get("range"))
        } else {
            continue;
        };
        let Some(path) = uri_to_path(uri) else {
            continue;
        };
        let sl = range
            .and_then(|r| r.pointer("/start/line"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        let sc = range
            .and_then(|r| r.pointer("/start/character"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        let el = range
            .and_then(|r| r.pointer("/end/line"))
            .and_then(Value::as_u64)
            .unwrap_or(sl as u64) as u32;
        let ec = range
            .and_then(|r| r.pointer("/end/character"))
            .and_then(Value::as_u64)
            .unwrap_or(sc as u64) as u32;
        let col = lines.col_1(&path, sl, sc);
        let end_col = lines.col_1(&path, el, ec);
        let preview = lines.preview(&path, sl);
        out.push(LocationItem {
            file: display_path(&path, base),
            line: sl + 1,
            col,
            end_line: el + 1,
            end_col,
            preview,
        });
    }
    out
}

fn extract_hover_markdown(result: &Value) -> Option<String> {
    let contents = result.get("contents")?;
    match contents {
        Value::String(s) => {
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        }
        Value::Object(obj) => {
            let val = obj.get("value").and_then(Value::as_str)?.trim();
            if val.is_empty() {
                return None;
            }
            if let Some(lang) = obj.get("language").and_then(Value::as_str) {
                Some(format!("```{lang}\n{val}\n```"))
            } else {
                Some(val.to_owned())
            }
        }
        Value::Array(arr) => {
            let mut parts = Vec::new();
            for item in arr {
                match item {
                    Value::String(s) if !s.trim().is_empty() => parts.push(s.trim().to_owned()),
                    Value::Object(obj) => {
                        if let Some(v) = obj.get("value").and_then(Value::as_str) {
                            let v = v.trim();
                            if !v.is_empty() {
                                if let Some(lang) = obj.get("language").and_then(Value::as_str) {
                                    parts.push(format!("```{lang}\n{v}\n```"));
                                } else {
                                    parts.push(v.to_owned());
                                }
                            }
                        }
                    }
                    _ => (),
                }
            }
            (!parts.is_empty()).then(|| parts.join("\n\n"))
        }
        _ => None,
    }
}

fn parse_document_symbols(
    arr: &[Value],
    file_path: &Path,
    display_file: &str,
    lines: &mut LineCache,
) -> Vec<SymbolItem> {
    let mut out = Vec::new();
    for item in arr {
        let Some(name) = item.get("name").and_then(Value::as_str) else {
            continue;
        };
        let kind = lsp_symbol_kind(item.get("kind").and_then(Value::as_u64).unwrap_or(0));
        let detail = item
            .get("detail")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let container = item
            .get("containerName")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let range = item
            .get("selectionRange")
            .or_else(|| item.get("range"))
            .or_else(|| item.pointer("/location/range"));
        let sl = range
            .and_then(|r| r.pointer("/start/line"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        let sc = range
            .and_then(|r| r.pointer("/start/character"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        let col = lines.col_1(file_path, sl, sc);
        let children = item
            .get("children")
            .and_then(Value::as_array)
            .map(|c| parse_document_symbols(c, file_path, display_file, lines))
            .unwrap_or_default();
        out.push(SymbolItem {
            name: name.to_owned(),
            kind: kind.to_owned(),
            file: display_file.to_owned(),
            line: sl + 1,
            col,
            detail,
            container,
            children,
        });
    }
    out
}

fn print_symbol_tree(items: &[SymbolItem], indent: usize) {
    let pad = "  ".repeat(indent);
    for s in items {
        let detail = s
            .detail
            .as_deref()
            .map(|d| format!("  {d}"))
            .unwrap_or_default();
        println!(
            "{pad}{:<7} {}  {}:{}:{}{detail}",
            s.kind, s.name, s.file, s.line, s.col
        );
        if !s.children.is_empty() {
            print_symbol_tree(&s.children, indent + 1);
        }
    }
}

// ---- `jato diag` -----------------------------------------------------------

fn push_pull_diags(
    items: &[Value],
    file_path: &Path,
    base: &Path,
    max_sev: u8,
    lines: &mut LineCache,
    out: &mut Vec<DiagItem>,
) {
    let display = display_path(file_path, base);
    for d in items {
        let sev_num = d.get("severity").and_then(Value::as_u64).unwrap_or(1) as u8;
        if sev_num > max_sev {
            continue;
        }
        let sl = d
            .pointer("/range/start/line")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        let sc = d
            .pointer("/range/start/character")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        let el = d
            .pointer("/range/end/line")
            .and_then(Value::as_u64)
            .unwrap_or(sl as u64) as u32;
        let ec = d
            .pointer("/range/end/character")
            .and_then(Value::as_u64)
            .unwrap_or(sc as u64) as u32;
        let code = match d.get("code") {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            Some(Value::Number(n)) => Some(n.to_string()),
            _ => None,
        };
        let source = d
            .get("source")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let message = d
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let col = lines.col_1(file_path, sl, sc);
        let end_col = lines.col_1(file_path, el, ec);
        out.push(DiagItem {
            file: display.clone(),
            line: sl + 1,
            col,
            end_line: el + 1,
            end_col,
            severity: lsp_severity_name(u64::from(sev_num)).to_owned(),
            code,
            source,
            message,
        });
    }
}

fn run_cargo_check_diags(
    workspace_root: &Path,
    manifest_path: Option<&Path>,
    filter_files: &[PathBuf],
    max_sev: u8,
    base: &Path,
    deadline: Instant,
    timeout_secs: u64,
) -> Result<(Vec<DiagItem>, bool, bool)> {
    let exe = super::own_jato_exe().context("locating current jato binary")?;
    let mut cmd = Command::new(&exe);
    cmd.arg("check")
        .arg("--workspace")
        .arg("--all-targets")
        .arg("--message-format=json")
        .current_dir(workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(mp) = manifest_path {
        cmd.arg("--manifest-path").arg(mp);
    }
    let mut child = cmd
        .spawn()
        .context("spawning `jato check` for diagnostics")?;
    let mut out_pipe = child.stdout.take().unwrap();
    let mut err_pipe = child.stderr.take().unwrap();
    let out_reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        buf
    });
    let err_reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
        buf
    });
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_reader.join();
            let _ = err_reader.join();
            bail!("timed out after {timeout_secs} s waiting for `jato check`");
        }
        thread::sleep(Duration::from_millis(25));
    };
    let stdout_bytes = out_reader.join().unwrap_or_default();
    let stderr_bytes = err_reader.join().unwrap_or_default();

    let filter_set: HashSet<PathBuf> = filter_files
        .iter()
        .map(|p| fs::canonicalize(p).unwrap_or_else(|_| p.clone()))
        .collect();

    let mut diags = Vec::new();
    let mut saw_filtered_error = false;
    let mut saw_other_error = false;
    let stdout = String::from_utf8_lossy(&stdout_bytes);
    for line in stdout.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("reason").and_then(Value::as_str) != Some("compiler-message") {
            continue;
        }
        let Some(msg) = val.get("message") else {
            continue;
        };
        let level = msg.get("level").and_then(Value::as_str).unwrap_or("");
        let sev_num: u8 = match level {
            "error" | "error: internal compiler error" => 1,
            "warning" => 2,
            "note" => 3,
            "help" => 4,
            _ => continue,
        };
        let text = msg
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let code = msg
            .pointer("/code/code")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let spans = msg
            .get("spans")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let primary = spans
            .iter()
            .find(|s| s.get("is_primary").and_then(Value::as_bool) == Some(true))
            .or_else(|| spans.first());
        let Some(span) = primary else {
            if text.starts_with("aborting due to")
                || text.contains("warning emitted")
                || text.contains("warnings emitted")
            {
                continue;
            }
            if sev_num == 1 {
                if filter_set.is_empty() {
                    saw_filtered_error = true;
                } else {
                    saw_other_error = true;
                }
            }
            if filter_set.is_empty() && sev_num <= max_sev {
                diags.push(DiagItem {
                    file: String::new(),
                    line: 0,
                    col: 0,
                    end_line: 0,
                    end_col: 0,
                    severity: lsp_severity_name(u64::from(sev_num)).to_owned(),
                    code,
                    source: Some("rustc".to_owned()),
                    message: text.to_owned(),
                });
            }
            continue;
        };
        let Some(fname) = span.get("file_name").and_then(Value::as_str) else {
            continue;
        };
        let full_path = {
            let p = Path::new(fname);
            let joined = if p.is_absolute() {
                p.to_path_buf()
            } else {
                workspace_root.join(p)
            };
            fs::canonicalize(&joined).unwrap_or(joined)
        };
        if !filter_set.is_empty() && !filter_set.contains(&full_path) {
            if sev_num == 1 {
                saw_other_error = true;
            }
            continue;
        }
        if sev_num == 1 {
            saw_filtered_error = true;
        }
        if sev_num > max_sev {
            continue;
        }
        let line_start = span.get("line_start").and_then(Value::as_u64).unwrap_or(1) as u32;
        let col_start = span
            .get("column_start")
            .and_then(Value::as_u64)
            .unwrap_or(1) as u32;
        let line_end = span
            .get("line_end")
            .and_then(Value::as_u64)
            .unwrap_or(line_start as u64) as u32;
        let col_end = span
            .get("column_end")
            .and_then(Value::as_u64)
            .unwrap_or(col_start as u64) as u32;
        diags.push(DiagItem {
            file: display_path(&full_path, base),
            line: line_start,
            col: col_start,
            end_line: line_end,
            end_col: col_end,
            severity: lsp_severity_name(u64::from(sev_num)).to_owned(),
            code,
            source: Some("rustc".to_owned()),
            message: text.to_owned(),
        });
    }
    diags.sort();
    diags.dedup();
    if !status.success() && !saw_filtered_error && !saw_other_error {
        let stderr = String::from_utf8_lossy(&stderr_bytes).trim().to_owned();
        if stderr.is_empty() {
            bail!("`jato check` failed with {status}");
        }
        bail!("`jato check` failed: {stderr}");
    }
    // If the workspace check failed (including due to errors in another crate
    // that prevented checking the requested file), fail the command cleanly.
    Ok((diags, !status.success(), saw_other_error))
}

pub fn cli_diag(args: &[OsString]) -> Result<i32> {
    let started = Instant::now();
    let mut workspace_all = false;
    let mut use_check = false;
    let mut max_sev: u8 = 2; // error + warning
    let mut json_out = false;
    let mut timeout_secs = DEFAULT_TIMEOUT_SECS;
    let mut raw_manifest: Option<PathBuf> = None;
    let mut files_arg: Vec<PathBuf> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let s = args[i].to_str().context("non-UTF-8 argument")?;
        match s {
            "-h" | "--help" => {
                print!("{DIAG_HELP}");
                return Ok(0);
            }
            "--workspace" | "--all" => workspace_all = true,
            "--check" | "--rustc" => use_check = true,
            "--errors-only" => max_sev = 1,
            "--json" => json_out = true,
            "--severity" => {
                let v = next_flag_value(args, &mut i, "--severity")?
                    .to_str()
                    .context("non-UTF-8 --severity")?;
                max_sev = severity_rank(v)?;
            }
            s if s.starts_with("--severity=") => {
                max_sev = severity_rank(&s["--severity=".len()..])?;
            }
            "--manifest-path" => {
                raw_manifest = Some(PathBuf::from(next_flag_value(
                    args,
                    &mut i,
                    "--manifest-path",
                )?));
            }
            s if s.starts_with("--manifest-path=") => {
                raw_manifest = Some(PathBuf::from(&s["--manifest-path=".len()..]));
            }
            "--timeout" => {
                let v = next_flag_value(args, &mut i, "--timeout")?
                    .to_str()
                    .context("non-UTF-8 --timeout")?;
                timeout_secs = parse_timeout(v)?;
            }
            s if s.starts_with("--timeout=") => {
                timeout_secs = parse_timeout(&s["--timeout=".len()..])?;
            }
            s if s.starts_with('-') => {
                bail!("unknown option `{s}` for `jato diag` (run `jato diag --help`)")
            }
            _ => files_arg.push(PathBuf::from(&args[i])),
        }
        i += 1;
    }

    let deadline = make_deadline(started, timeout_secs);
    let cwd = env::current_dir()?;
    let canon_cwd = fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone());
    let manifest_path = raw_manifest
        .as_deref()
        .map(|mp| resolve_manifest_path(&cwd, mp))
        .transpose()?;
    let first_hint = manifest_path
        .as_deref()
        .or_else(|| files_arg.first().map(PathBuf::as_path));
    let workspace_root = resolve_workspace_root(first_hint)?;

    let mut target_files: Vec<PathBuf> = Vec::new();
    for f in &files_arg {
        let abs = if f.is_absolute() {
            f.clone()
        } else {
            cwd.join(f)
        };
        if !abs.exists() {
            bail!("file not found: {}", f.display());
        }
        let canon = fs::canonicalize(&abs).unwrap_or(abs);
        if canon.is_dir() {
            collect_rs_files(&canon, &mut target_files);
        } else {
            target_files.push(canon);
        }
    }
    target_files.sort();
    target_files.dedup();

    let (diags, check_failed, other_files_failed) = if use_check {
        run_cargo_check_diags(
            &workspace_root,
            manifest_path.as_deref(),
            &target_files,
            max_sev,
            &canon_cwd,
            deadline,
            timeout_secs,
        )?
    } else {
        let modified_in_git = git_modified_rs_files(&workspace_root).unwrap_or_default();
        if target_files.is_empty() {
            if workspace_all {
                collect_rs_files(&workspace_root, &mut target_files);
            } else if !modified_in_git.is_empty() {
                target_files = modified_in_git.clone();
            } else {
                let fallback_dir = manifest_path
                    .as_deref()
                    .and_then(Path::parent)
                    .unwrap_or(&canon_cwd);
                let crate_root = nearest_crate_root(fallback_dir);
                collect_rs_files(&crate_root, &mut target_files);
            }
        }
        let mut collected = Vec::new();
        if !target_files.is_empty() {
            let mut sess = CliLspSession::connect(&workspace_root, deadline, timeout_secs)?;
            let mut lines = LineCache::new();

            // Notify rust-analyzer synchronously of any sibling git-modified
            // `.rs` files outside `target_files` so inter-file diagnostics
            // never miss a recent disk edit awaiting inotify debounce.
            let target_set: HashSet<&Path> = target_files.iter().map(PathBuf::as_path).collect();
            let watched_changes: Vec<Value> = modified_in_git
                .iter()
                .filter(|p| !target_set.contains(p.as_path()))
                .map(|p| json!({ "uri": path_to_uri(p), "type": 2 }))
                .collect();
            if !watched_changes.is_empty() {
                let _ = sess.notify(
                    "workspace/didChangeWatchedFiles",
                    json!({ "changes": watched_changes }),
                );
            }

            if target_files.len() <= 16 {
                // Open all target files first so cross-file edits among them
                // are visible before pulling diagnostics on any single file.
                let mut uris = Vec::with_capacity(target_files.len());
                for file in &target_files {
                    let (uri, _) = sess.open_file(file)?;
                    uris.push((file, uri));
                }
                for (file, uri) in uris {
                    let res = sess.request(
                        "textDocument/diagnostic",
                        json!({ "textDocument": { "uri": uri } }),
                    )?;
                    if let Some(items) = res.get("items").and_then(Value::as_array) {
                        push_pull_diags(
                            items,
                            file,
                            &canon_cwd,
                            max_sev,
                            &mut lines,
                            &mut collected,
                        );
                    }
                }
            } else {
                for file in &target_files {
                    let (uri, _) = sess.open_file(file)?;
                    let res = sess.request(
                        "textDocument/diagnostic",
                        json!({ "textDocument": { "uri": uri } }),
                    )?;
                    if let Some(items) = res.get("items").and_then(Value::as_array) {
                        push_pull_diags(
                            items,
                            file,
                            &canon_cwd,
                            max_sev,
                            &mut lines,
                            &mut collected,
                        );
                    }
                    sess.close_file(&uri);
                }
            }
            sess.close();
        }
        collected.sort();
        collected.dedup();
        (collected, false, false)
    };

    let errors = diags.iter().filter(|d| d.severity == "error").count();
    let warnings = diags.iter().filter(|d| d.severity == "warning").count();
    let elapsed_ms = started.elapsed().as_millis() as u64;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "workspace": workspace_root,
                "mode": if use_check { "check" } else { "rust-analyzer" },
                "files_checked": if use_check && target_files.is_empty() { Value::Null } else { json!(target_files.len()) },
                "errors": errors,
                "warnings": warnings,
                "other_files_failed": other_files_failed,
                "elapsed_ms": elapsed_ms,
                "diagnostics": diags,
            }))?
        );
    } else {
        for d in &diags {
            let loc = if d.file.is_empty() {
                String::new()
            } else {
                format!("{}:{}:{}: ", d.file, d.line, d.col)
            };
            let code = d
                .code
                .as_deref()
                .map(|c| format!("[{c}]"))
                .unwrap_or_default();
            println!("{loc}{}{code}: {}", d.severity, d.message);
        }
        if diags.is_empty() {
            println!("0 errors, 0 warnings ({elapsed_ms} ms)");
        } else {
            println!(
                "{errors} {}, {warnings} {} ({elapsed_ms} ms)",
                if errors == 1 { "error" } else { "errors" },
                if warnings == 1 { "warning" } else { "warnings" }
            );
        }
        if other_files_failed && errors == 0 {
            eprintln!("note: workspace check failed due to compiler errors in other files");
        }
    }
    Ok(if errors > 0 || check_failed { 1 } else { 0 })
}

// ---- `jato def` ------------------------------------------------------------

pub fn cli_def(args: &[OsString]) -> Result<i32> {
    let started = Instant::now();
    let mut method = "textDocument/definition";
    let mut json_out = false;
    let mut timeout_secs = DEFAULT_TIMEOUT_SECS;
    let mut positional = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let s = args[i].to_str().context("non-UTF-8 argument")?;
        match s {
            "-h" | "--help" => {
                print!("{DEF_HELP}");
                return Ok(0);
            }
            "--type" => method = "textDocument/typeDefinition",
            "--impl" => method = "textDocument/implementation",
            "--json" => json_out = true,
            "--timeout" => {
                let v = next_flag_value(args, &mut i, "--timeout")?
                    .to_str()
                    .context("non-UTF-8 --timeout")?;
                timeout_secs = parse_timeout(v)?;
            }
            s if s.starts_with("--timeout=") => {
                timeout_secs = parse_timeout(&s["--timeout=".len()..])?;
            }
            s if s.starts_with('-') => {
                bail!("unknown option `{s}` for `jato def` (run `jato def --help`)")
            }
            _ => positional.push(s.to_owned()),
        }
        i += 1;
    }

    let deadline = make_deadline(started, timeout_secs);
    let loc = parse_location_args(&positional)?;
    let cwd = env::current_dir()?;
    let canon_cwd = fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone());
    let file_path = if loc.path.is_absolute() {
        loc.path
    } else {
        cwd.join(loc.path)
    };
    if !file_path.is_file() {
        bail!("file not found: {}", file_path.display());
    }
    let workspace_root = resolve_workspace_root(Some(&file_path))?;

    let mut sess = CliLspSession::connect(&workspace_root, deadline, timeout_secs)?;
    let (uri, text) = sess.open_file(&file_path)?;
    let (line_0, utf16_col) = char_pos_to_lsp(&text, loc.line, loc.col);
    let res = sess.request(
        method,
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": line_0, "character": utf16_col }
        }),
    )?;
    sess.close();

    let mut lines = LineCache::new();
    let locations = parse_lsp_locations(&res, &canon_cwd, &mut lines);
    if json_out {
        println!("{}", serde_json::to_string_pretty(&locations)?);
    } else if locations.is_empty() {
        println!("No definition found.");
    } else {
        for item in &locations {
            match &item.preview {
                Some(p) if !p.is_empty() => {
                    println!("{}:{}:{}: {p}", item.file, item.line, item.col)
                }
                _ => println!("{}:{}:{}", item.file, item.line, item.col),
            }
        }
    }
    Ok(0)
}

// ---- `jato refs` -----------------------------------------------------------

pub fn cli_refs(args: &[OsString]) -> Result<i32> {
    let started = Instant::now();
    let mut include_decl = true;
    let mut limit_arg: Option<usize> = None;
    let mut json_out = false;
    let mut timeout_secs = DEFAULT_TIMEOUT_SECS;
    let mut positional = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let s = args[i].to_str().context("non-UTF-8 argument")?;
        match s {
            "-h" | "--help" => {
                print!("{REFS_HELP}");
                return Ok(0);
            }
            "--no-decl" => include_decl = false,
            "--json" => json_out = true,
            "--limit" => {
                let v = next_flag_value(args, &mut i, "--limit")?
                    .to_str()
                    .context("non-UTF-8 --limit")?;
                limit_arg = Some(
                    v.parse()
                        .context("--limit expects a non-negative integer")?,
                );
            }
            s if s.starts_with("--limit=") => {
                limit_arg = Some(
                    s["--limit=".len()..]
                        .parse()
                        .context("--limit expects a non-negative integer")?,
                );
            }
            "--timeout" => {
                let v = next_flag_value(args, &mut i, "--timeout")?
                    .to_str()
                    .context("non-UTF-8 --timeout")?;
                timeout_secs = parse_timeout(v)?;
            }
            s if s.starts_with("--timeout=") => {
                timeout_secs = parse_timeout(&s["--timeout=".len()..])?;
            }
            s if s.starts_with('-') => {
                bail!("unknown option `{s}` for `jato refs` (run `jato refs --help`)")
            }
            _ => positional.push(s.to_owned()),
        }
        i += 1;
    }

    let limit = limit_arg.unwrap_or(if json_out { 0 } else { 200 });
    let deadline = make_deadline(started, timeout_secs);
    let loc = parse_location_args(&positional)?;
    let cwd = env::current_dir()?;
    let canon_cwd = fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone());
    let file_path = if loc.path.is_absolute() {
        loc.path
    } else {
        cwd.join(loc.path)
    };
    if !file_path.is_file() {
        bail!("file not found: {}", file_path.display());
    }
    let workspace_root = resolve_workspace_root(Some(&file_path))?;

    let mut sess = CliLspSession::connect(&workspace_root, deadline, timeout_secs)?;
    let (uri, text) = sess.open_file(&file_path)?;
    let (line_0, utf16_col) = char_pos_to_lsp(&text, loc.line, loc.col);
    let res = sess.request(
        "textDocument/references",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": line_0, "character": utf16_col },
            "context": { "includeDeclaration": include_decl }
        }),
    )?;
    sess.close();

    let mut lines = LineCache::new();
    let mut locations = parse_lsp_locations(&res, &canon_cwd, &mut lines);
    locations.sort_by(|a, b| (&a.file, a.line, a.col).cmp(&(&b.file, b.line, b.col)));
    let total = locations.len();
    if limit > 0 && locations.len() > limit {
        locations.truncate(limit);
    }
    if json_out {
        println!("{}", serde_json::to_string_pretty(&locations)?);
    } else if locations.is_empty() {
        println!("No references found.");
    } else {
        for item in &locations {
            match &item.preview {
                Some(p) if !p.is_empty() => {
                    println!("{}:{}:{}: {p}", item.file, item.line, item.col)
                }
                _ => println!("{}:{}:{}", item.file, item.line, item.col),
            }
        }
        if total > locations.len() {
            println!(
                "... {} more reference(s) omitted (pass --limit 0 to show all)",
                total - locations.len()
            );
        }
    }
    Ok(0)
}

// ---- `jato hover` ----------------------------------------------------------

pub fn cli_hover(args: &[OsString]) -> Result<i32> {
    let started = Instant::now();
    let mut json_out = false;
    let mut timeout_secs = DEFAULT_TIMEOUT_SECS;
    let mut positional = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let s = args[i].to_str().context("non-UTF-8 argument")?;
        match s {
            "-h" | "--help" => {
                print!("{HOVER_HELP}");
                return Ok(0);
            }
            "--json" => json_out = true,
            "--timeout" => {
                let v = next_flag_value(args, &mut i, "--timeout")?
                    .to_str()
                    .context("non-UTF-8 --timeout")?;
                timeout_secs = parse_timeout(v)?;
            }
            s if s.starts_with("--timeout=") => {
                timeout_secs = parse_timeout(&s["--timeout=".len()..])?;
            }
            s if s.starts_with('-') => {
                bail!("unknown option `{s}` for `jato hover` (run `jato hover --help`)")
            }
            _ => positional.push(s.to_owned()),
        }
        i += 1;
    }

    let deadline = make_deadline(started, timeout_secs);
    let loc = parse_location_args(&positional)?;
    let cwd = env::current_dir()?;
    let file_path = if loc.path.is_absolute() {
        loc.path
    } else {
        cwd.join(loc.path)
    };
    if !file_path.is_file() {
        bail!("file not found: {}", file_path.display());
    }
    let workspace_root = resolve_workspace_root(Some(&file_path))?;

    let mut sess = CliLspSession::connect(&workspace_root, deadline, timeout_secs)?;
    let (uri, text) = sess.open_file(&file_path)?;
    let (line_0, utf16_col) = char_pos_to_lsp(&text, loc.line, loc.col);
    let res = sess.request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": line_0, "character": utf16_col }
        }),
    )?;
    sess.close();

    let markdown = extract_hover_markdown(&res);
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "contents": markdown,
                "raw": res
            }))?
        );
    } else if let Some(md) = markdown {
        println!("{md}");
    } else {
        println!("No hover information.");
    }
    Ok(0)
}

// ---- `jato symbols` --------------------------------------------------------

pub fn cli_symbols(args: &[OsString]) -> Result<i32> {
    let started = Instant::now();
    let mut query: Option<String> = None;
    let mut raw_manifest: Option<PathBuf> = None;
    let mut limit_arg: Option<usize> = None;
    let mut json_out = false;
    let mut timeout_secs = DEFAULT_TIMEOUT_SECS;
    let mut positional: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let s = args[i].to_str().context("non-UTF-8 argument")?;
        match s {
            "-h" | "--help" => {
                print!("{SYMBOLS_HELP}");
                return Ok(0);
            }
            "--json" => json_out = true,
            "-q" | "--query" | "-w" | "--workspace" => {
                let v = next_flag_value(args, &mut i, s)?
                    .to_str()
                    .context("non-UTF-8 --query")?;
                query = Some(v.to_owned());
            }
            s if s.starts_with("--query=") => {
                query = Some(s["--query=".len()..].to_owned());
            }
            s if s.starts_with("--workspace=") => {
                query = Some(s["--workspace=".len()..].to_owned());
            }
            "--manifest-path" => {
                raw_manifest = Some(PathBuf::from(next_flag_value(
                    args,
                    &mut i,
                    "--manifest-path",
                )?));
            }
            s if s.starts_with("--manifest-path=") => {
                raw_manifest = Some(PathBuf::from(&s["--manifest-path=".len()..]));
            }
            "--limit" => {
                let v = next_flag_value(args, &mut i, "--limit")?
                    .to_str()
                    .context("non-UTF-8 --limit")?;
                limit_arg = Some(
                    v.parse()
                        .context("--limit expects a non-negative integer")?,
                );
            }
            s if s.starts_with("--limit=") => {
                limit_arg = Some(
                    s["--limit=".len()..]
                        .parse()
                        .context("--limit expects a non-negative integer")?,
                );
            }
            "--timeout" => {
                let v = next_flag_value(args, &mut i, "--timeout")?
                    .to_str()
                    .context("non-UTF-8 --timeout")?;
                timeout_secs = parse_timeout(v)?;
            }
            s if s.starts_with("--timeout=") => {
                timeout_secs = parse_timeout(&s["--timeout=".len()..])?;
            }
            s if s.starts_with('-') => {
                bail!("unknown option `{s}` for `jato symbols` (run `jato symbols --help`)")
            }
            _ => {
                if positional.is_some() {
                    bail!("unexpected extra argument `{s}` (run `jato symbols --help`)");
                }
                positional = Some(s.to_owned());
            }
        }
        i += 1;
    }

    if query.is_some() && positional.is_some() {
        bail!("cannot combine a positional FILE/QUERY with `--query` (run `jato symbols --help`)");
    }

    let limit = limit_arg.unwrap_or(if json_out { 0 } else { 200 });
    let deadline = make_deadline(started, timeout_secs);
    let cwd = env::current_dir()?;
    let canon_cwd = fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone());
    let manifest_path = raw_manifest
        .as_deref()
        .map(|mp| resolve_manifest_path(&cwd, mp))
        .transpose()?;

    // Decide documentSymbol vs workspace/symbol.
    let file_mode = if query.is_some() {
        None
    } else if let Some(ref pos) = positional {
        let candidate = if Path::new(pos).is_absolute() {
            PathBuf::from(pos)
        } else {
            cwd.join(pos)
        };
        if candidate.is_file() || pos.ends_with(".rs") {
            if !candidate.is_file() {
                bail!("file not found: {pos}");
            }
            Some(candidate)
        } else {
            query = Some(pos.clone());
            None
        }
    } else {
        bail!("expected a FILE path or `--query QUERY` (run `jato symbols --help`)");
    };

    let mut lines = LineCache::new();
    if let Some(file_path) = file_mode {
        let canon = fs::canonicalize(&file_path).unwrap_or(file_path);
        let workspace_root = resolve_workspace_root(Some(&canon))?;
        let mut sess = CliLspSession::connect(&workspace_root, deadline, timeout_secs)?;
        let (uri, _) = sess.open_file(&canon)?;
        let res = sess.request(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": uri } }),
        )?;
        sess.close();

        let disp = display_path(&canon, &canon_cwd);
        let items = res
            .as_array()
            .map(|a| parse_document_symbols(a, &canon, &disp, &mut lines))
            .unwrap_or_default();
        if json_out {
            println!("{}", serde_json::to_string_pretty(&items)?);
        } else if items.is_empty() {
            println!("No symbols found.");
        } else {
            print_symbol_tree(&items, 0);
        }
        return Ok(0);
    }

    let q = query.unwrap_or_default();
    let workspace_root = resolve_workspace_root(manifest_path.as_deref())?;
    let mut sess = CliLspSession::connect(&workspace_root, deadline, timeout_secs)?;
    let res = sess.request("workspace/symbol", json!({ "query": q }))?;
    sess.close();

    let mut symbols = Vec::new();
    if let Some(arr) = res.as_array() {
        for item in arr {
            let Some(name) = item.get("name").and_then(Value::as_str) else {
                continue;
            };
            let Some(uri) = item.pointer("/location/uri").and_then(Value::as_str) else {
                continue;
            };
            let Some(path) = uri_to_path(uri) else {
                continue;
            };
            let sl = item
                .pointer("/location/range/start/line")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32;
            let sc = item
                .pointer("/location/range/start/character")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32;
            let col = lines.col_1(&path, sl, sc);
            let kind = lsp_symbol_kind(item.get("kind").and_then(Value::as_u64).unwrap_or(0));
            let container = item
                .get("containerName")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            symbols.push(SymbolItem {
                name: name.to_owned(),
                kind: kind.to_owned(),
                file: display_path(&path, &canon_cwd),
                line: sl + 1,
                col,
                detail: None,
                container,
                children: Vec::new(),
            });
        }
    }
    let total = symbols.len();
    if limit > 0 && symbols.len() > limit {
        symbols.truncate(limit);
    }
    if json_out {
        println!("{}", serde_json::to_string_pretty(&symbols)?);
    } else if symbols.is_empty() {
        println!("No matching workspace symbols.");
    } else {
        for s in &symbols {
            let cont = s
                .container
                .as_deref()
                .map(|c| format!("  (in {c})"))
                .unwrap_or_default();
            println!(
                "{:<7} {}  {}:{}:{}{cont}",
                s.kind, s.name, s.file, s.line, s.col
            );
        }
        if total > symbols.len() {
            println!(
                "... {} more symbol(s) omitted (pass --limit 0 to show all)",
                total - symbols.len()
            );
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_location_specs_and_separate_args() {
        assert_eq!(
            parse_location_args(&["src/lib.rs:42:9".into()]).unwrap(),
            ParsedLocation {
                path: PathBuf::from("src/lib.rs"),
                line: 42,
                col: 9,
            }
        );
        assert_eq!(
            parse_location_args(&["src/lib.rs:42".into()]).unwrap(),
            ParsedLocation {
                path: PathBuf::from("src/lib.rs"),
                line: 42,
                col: 1,
            }
        );
        assert_eq!(
            parse_location_args(&["src/lib.rs".into(), "7".into(), "12".into()]).unwrap(),
            ParsedLocation {
                path: PathBuf::from("src/lib.rs"),
                line: 7,
                col: 12,
            }
        );
        assert!(parse_location_args(&["src/lib.rs:0:5".into()]).is_err());
        assert!(parse_location_args(&["src/lib.rs".into()]).is_err());
    }

    #[test]
    fn converts_utf16_columns_accurately_across_surrogate_pairs() {
        // '🦀' is 1 char (4 UTF-8 bytes, 2 UTF-16 code units).
        let text = "let x = \"🦀\";\nlet y = 1;\n";
        // Line 1, col 11 ('"' after crab): chars before = `let x = "🦀` (10 chars, 11 UTF-16 units)
        assert_eq!(char_pos_to_lsp(text, 1, 11), (0, 11));
        assert_eq!(lsp_utf16_to_col_1("let x = \"🦀\";", 11), 11);
        assert_eq!(char_pos_to_lsp(text, 2, 5), (1, 4));
    }

    #[test]
    fn extracts_hover_from_all_lsp_shapes() {
        assert_eq!(
            extract_hover_markdown(&json!({
                "contents": { "kind": "markdown", "value": "```rust\nfn foo()\n```" }
            })),
            Some("```rust\nfn foo()\n```".to_owned())
        );
        assert_eq!(
            extract_hover_markdown(&json!({
                "contents": [{ "language": "rust", "value": "pub struct A" }, "doc"]
            })),
            Some("```rust\npub struct A\n```\n\ndoc".to_owned())
        );
        assert_eq!(extract_hover_markdown(&Value::Null), None);
    }

    #[test]
    fn git_porcelain_z_handles_deleted_renames_without_desync() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = fs::canonicalize(tmp.path()).unwrap();
        fs::create_dir_all(ws.join("src")).unwrap();
        let kept = ws.join("src/kept.rs");
        fs::write(&kept, "fn ok() {}\n").unwrap();

        // RD = renamed in index, deleted in worktree: emits new path + NUL + old path + NUL,
        // followed by a normal modified entry ` M src/kept.rs`.
        let raw = b"RD src/gone.rs\0src/old.rs\0 M src/kept.rs\0";
        let got = parse_git_porcelain_z(raw, &ws, &ws);
        assert_eq!(got, vec![kept]);
    }
}
