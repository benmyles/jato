//! Observed Cargo build-script execution caching.
//!
//! When a supervised Cargo build runs, `libjato_script.so` intercepts Cargo's
//! execution of `<TARGET>/.../build/.../build-script-*` and hands off to
//! `jato __script <exe> [args...]`. That process asks the active build
//! supervisor (`JATO_SESSION`) to look up or record the build script keyed on
//! the build-script binary's content digest, arguments, working directory, and
//! normalized environment, verified against every file, directory, symlink,
//! subprocess, and nested C/Rust tool compilation observed while the script
//! ran. Passing, hermetic scripts whose surviving writes stay inside `OUT_DIR`
//! are cached and replayed (restoring `OUT_DIR` outputs, Cargo stdout
//! directives, and stderr) across clean builds, checkouts, and config changes,
//! while attributing all inputs and outputs to the enclosing whole build.
use crate::cas::{Placing, Staged, Store};
use crate::command::{
    self, Outcome, Ran, decode_detail, encode_detail, exit_code, field, harmless_socket, log,
    own_environment, tee, umask,
};
use crate::manifest::{Env, Here, Input, Manifest, Output, Place, State, path_dirs, which};
use crate::observe::{self, Kind, Report, Stamp};
use crate::roots::{self, Roots, Surroundings};
use crate::scan::Digest;
use crate::wire::{self, Reader, Writer};
use anyhow::Result;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::{SocketAddr, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Instant;

const SCRIPT_EPOCH: &str = "jato build script v1";

pub const REQ_SCRIPT: u8 = 1;
pub const HIT: u8 = 0;
pub const RUN: u8 = 1;
pub const DECLINE: u8 = 2;
pub const RUN_OWN: u8 = 5;

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
const SCRIPT_SHIM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/libjato_script.so"));
#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
const SCRIPT_SHIM: &[u8] = &[];

fn os(bytes: &[u8]) -> OsString {
    OsString::from_vec(bytes.to_vec())
}

fn address(name: &OsStr) -> io::Result<SocketAddr> {
    SocketAddr::from_abstract_name(name.as_bytes())
}

/// Extract `libjato_script.so` into the cache directory atomically, or `None`
/// if build-script caching is disabled or unsupported.
pub fn library(cache: &Path) -> Option<PathBuf> {
    if SCRIPT_SHIM.is_empty()
        || env::var_os("JATO_CACHE_SCRIPTS").is_some_and(|v| v == "0")
        || env::var_os("JATO_DISABLE").is_some_and(|v| v == "1")
    {
        return None;
    }
    static NAME: OnceLock<String> = OnceLock::new();
    let name = NAME.get_or_init(|| {
        format!(
            "jato-script-{}.so",
            &blake3::hash(SCRIPT_SHIM).to_hex().as_str()[..16]
        )
    });
    let path = cache.join("lib").join(name);
    if fs::metadata(&path).is_ok_and(|meta| meta.len() == SCRIPT_SHIM.len() as u64) {
        return Some(path);
    }
    fs::create_dir_all(path.parent()?).ok()?;
    let making = path.with_extension(format!("s{}", std::process::id()));
    fs::write(&making, SCRIPT_SHIM).ok()?;
    if fs::rename(&making, &path).is_err() {
        let _ = fs::remove_file(&making);
    }
    path.is_file().then_some(path)
}

/// Attach `libjato_script.so` and its environment markers to a supervised
/// Cargo command so build scripts hand off to `jato __script`.
pub fn offer(cmd: &mut Command, cache: &Path, target: &Path) {
    let Some(lib) = library(cache) else {
        return;
    };
    let Ok(bin) = env::current_exe() else {
        return;
    };
    let canonical = fs::canonicalize(target).unwrap_or_else(|_| target.to_owned());
    cmd.env("JATO_SCRIPT_BIN", bin);
    cmd.env("JATO_SCRIPT_TARGET", canonical);
    let given = cmd
        .get_envs()
        .find(|(name, _)| *name == "LD_PRELOAD")
        .map(|(_, value)| value.map(OsStr::to_owned))
        .unwrap_or_else(|| env::var_os("LD_PRELOAD"));
    let mut load = lib.as_os_str().to_owned();
    if let Some(others) = given {
        for other in others
            .as_bytes()
            .split(|b| *b == b':' || *b == b' ')
            .filter(|other| !other.is_empty())
            .filter(|other| !observe::is_script_library(Path::new(OsStr::from_bytes(other))))
        {
            load.push(":");
            load.push(OsStr::from_bytes(other));
        }
    }
    cmd.env("LD_PRELOAD", load);
}

/// Remove build-script preload variables from a fallback command.
pub fn withdraw(cmd: &mut Command) {
    cmd.env_remove("JATO_SCRIPT_BIN")
        .env_remove("JATO_SCRIPT_TARGET")
        .env_remove("JATO_SCRIPT_DIRECT");
    let given = cmd
        .get_envs()
        .find(|(name, _)| *name == "LD_PRELOAD")
        .map(|(_, value)| value.map(OsStr::to_owned))
        .unwrap_or_else(|| env::var_os("LD_PRELOAD"));
    if let Some(others) = given {
        let kept: Vec<&[u8]> = others
            .as_bytes()
            .split(|b| *b == b':' || *b == b' ')
            .filter(|entry| !entry.is_empty())
            .filter(|entry| !observe::is_script_library(Path::new(OsStr::from_bytes(entry))))
            .collect();
        if kept.is_empty() {
            cmd.env_remove("LD_PRELOAD");
        } else {
            cmd.env("LD_PRELOAD", OsString::from_vec(kept.join(&b':')));
        }
    }
}

pub struct ScriptRequest {
    pub cwd: PathBuf,
    pub binary: PathBuf,
    pub args: Vec<OsString>,
    pub env: Env,
}

#[derive(Default)]
pub struct ScriptChildren {
    pub parts: Vec<(Manifest, Roots)>,
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

pub fn script_roots(req: &ScriptRequest, target: Option<&Path>) -> Roots {
    let named = target.map(Path::to_owned).or_else(|| {
        ["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"]
            .iter()
            .find_map(|name| req.env.get(OsStr::new(name)))
            .map(|path| req.cwd.join(path))
    });
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

pub fn script_recipe(req: &ScriptRequest, roots: &Roots, binary_digest: &Digest) -> Digest {
    let mut hash = blake3::Hasher::new_derive_key(SCRIPT_EPOCH);
    hash.update(binary_digest);
    field(&mut hash, &roots.mark_os(req.cwd.as_os_str()));
    field(&mut hash, &(req.args.len() as u64).to_le_bytes());
    for arg in &req.args {
        field(&mut hash, &roots.mark_os(arg));
    }
    let stand_ins = req.env.get(OsStr::new(crate::tools::DIRECTORY));
    field(&mut hash, b"environment");
    for (name, value) in &req.env {
        if excluded_env(name.as_bytes()) {
            continue;
        }
        field(&mut hash, name.as_bytes());
        if name == "PATH"
            && let Some(dir) = stand_ins
        {
            let cleaned = crate::tools::without(value, dir);
            field(&mut hash, &roots.mark_os(&cleaned));
        } else {
            field(&mut hash, &roots.mark_os(value));
        }
    }
    field(&mut hash, &umask().to_le_bytes());
    *hash.finalize().as_bytes()
}

pub fn script_label(req: &ScriptRequest) -> String {
    if let Some(pkg) = req
        .env
        .get(OsStr::new("CARGO_PKG_NAME"))
        .and_then(|v| v.to_str())
    {
        return format!("script:{pkg}");
    }
    let name = req
        .binary
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("build-script");
    format!("script:{name}")
}

fn out_dir_of(req: &ScriptRequest) -> Option<PathBuf> {
    req.env
        .get(OsStr::new("OUT_DIR"))
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Remove files in `out_dir` that are not part of the restored manifest,
/// pruning emptied subdirectories beneath `out_dir`.
fn prune_out_dir(out_dir: &Path, keep: &HashSet<PathBuf>) {
    fn walk(dir: &Path, keep: &HashSet<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                walk(&path, keep);
                let _ = fs::remove_dir(&path);
            } else if !keep.contains(&path) {
                let _ = fs::remove_file(&path);
            }
        }
    }
    if out_dir.is_dir() {
        walk(out_dir, keep);
    }
}

/// Look up a cached passing build-script execution.
pub fn lookup(
    req: &ScriptRequest,
    roots: &Roots,
    recipe: &Digest,
    store: &mut Store,
) -> Result<Option<(Outcome, Manifest)>> {
    let Some(out_dir) = out_dir_of(req) else {
        return Ok(None);
    };
    store.begin_read();
    let candidates = store.candidates(recipe)?;
    if candidates.is_empty() {
        store.end_read();
        return Ok(None);
    }
    let path = path_dirs(&req.cwd, &req.env);
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
        if manifest.placed(roots).is_err() || manifest.holds(&mut here, store).is_err() {
            continue;
        }
        let mut damaged = false;
        let restored = store.restore(
            candidate,
            &manifest,
            roots,
            Placing::default(),
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
        let keep: HashSet<PathBuf> = manifest
            .outputs
            .iter()
            .filter_map(|o| roots.join(o.root, &o.path))
            .collect();
        prune_out_dir(&out_dir, &keep);
        store.count("script_hit", 1);
        store.count("saved_ms", restored.cost_ms);
        log(&script_label(req), "hit");
        matched = Some((
            Outcome {
                code,
                stdout: stdout.into_owned(),
                stderr: stderr.into_owned(),
            },
            manifest,
        ));
        break;
    }
    store.end_read();
    for key in forget {
        store.forget(&key);
    }
    Ok(matched)
}

/// Record a passing, hermetic build-script execution whose surviving writes
/// stay inside `OUT_DIR`.
pub fn record(
    req: &ScriptRequest,
    roots: &Roots,
    recipe: &Digest,
    ran: &Ran,
    report: &Report,
    children: &ScriptChildren,
    store: &mut Store,
) -> Result<Manifest, String> {
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
        return Err("a child process outlived the build script".into());
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
    let Some(out_dir) = out_dir_of(req) else {
        return Err("OUT_DIR is not set".into());
    };
    let canon_out = fs::canonicalize(&out_dir).ok();
    let in_out_dir = |path: &Path| -> bool {
        path.starts_with(&out_dir)
            || canon_out.as_ref().is_some_and(|c| path.starts_with(c))
            || fs::canonicalize(path)
                .ok()
                .is_some_and(|p| canon_out.as_ref().is_some_and(|c| p.starts_with(c)))
    };

    let mut manifest = Manifest::default();
    let mut made: HashSet<Place> = HashSet::new();
    let mut outputs: Vec<(u8, PathBuf, PathBuf)> = Vec::new();
    let mut output_set: HashSet<Place> = HashSet::new();

    let mut record_made = |place: Place| {
        if place.0 == roots::TARGET {
            for dir in place.1.ancestors().skip(1) {
                if dir.as_os_str().is_empty() || !made.insert((place.0, dir.to_owned())) {
                    break;
                }
            }
        }
        made.insert(place);
    };

    // Outputs written by cached nested compilers/tools (e.g. `cc` -> `OUT_DIR/foo.o`).
    let mut child_inputs: HashMap<Place, (Option<Vec<u8>>, State)> = HashMap::new();
    let mut child_which: HashSet<Vec<u8>> = HashSet::new();
    let mut child_pins: BTreeSet<Place> = BTreeSet::new();
    for (child_manifest, child_roots) in &children.parts {
        let translate = |root: u8, path: &Path| -> Option<Place> {
            let abs = child_roots.join(root, path)?;
            let (r, rel) = roots.split(&abs);
            Some((r, rel.to_owned()))
        };
        for out in &child_manifest.outputs {
            let Some(abs) = child_roots.join(out.root, &out.path) else {
                return Err("child compiler output could not be located".into());
            };
            let (r, rel) = roots.split(&abs);
            let place = (r, rel.to_owned());
            record_made(place.clone());
            if abs.is_file() {
                if !in_out_dir(&abs) {
                    return Err(format!(
                        "child compiler wrote {} outside OUT_DIR",
                        abs.display()
                    ));
                }
                if output_set.insert(place) {
                    outputs.push((r, rel.to_owned(), abs));
                }
            }
        }
        for input in &child_manifest.inputs {
            let Some(place) = translate(input.root, &input.path) else {
                return Err("child compiler input could not be located".into());
            };
            let link = match &input.link {
                Some(link) => {
                    let raw = child_roots
                        .unmark(link)
                        .ok_or_else(|| "child link could not be unmarked".to_owned())?;
                    Some(roots.mark(&raw).into_owned())
                }
                None => None,
            };
            child_inputs
                .entry(place)
                .or_insert((link, input.state.clone()));
        }
        for (name, _) in &child_manifest.which {
            child_which.insert(name.clone());
        }
        for (_, named) in &child_manifest.pins {
            for id in [roots::WORK, roots::TARGET] {
                if let Some(root) = roots.path(id)
                    && named.starts_with(root)
                {
                    child_pins.insert((id, root.to_owned()));
                }
            }
        }
    }

    let mut executed = Vec::new();
    let mut listings: HashMap<Place, Vec<OsString>> = HashMap::new();
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

    // Pre-register all paths written by the script itself so `sub/../gen.h`
    // reads of generated files inside OUT_DIR are treated as internal.
    for &(path, access) in &paths {
        if access.written {
            let (root, rel) = roots.split(path);
            record_made((root, rel.to_owned()));
        }
    }

    let was_made = |place: &Place| -> bool {
        made.contains(place)
            || (place.0 == roots::TARGET
                && place
                    .1
                    .components()
                    .any(|c| c == std::path::Component::ParentDir)
                && roots
                    .join(place.0, &place.1)
                    .and_then(|p| fs::canonicalize(p).ok())
                    .is_some_and(|canon| {
                        let (cr, crel) = roots.split(&canon);
                        cr == roots::TARGET && made.contains(&(cr, crel.to_owned()))
                    }))
    };

    for (path, access) in paths {
        // The build-script binary is already keyed by digest in `recipe`.
        if *path == req.binary {
            continue;
        }
        let (root, rel) = roots.split(path);
        let place = (root, rel.to_owned());
        let now = fs::symlink_metadata(path);
        if access.written {
            if access.read {
                return Err(format!(
                    "it changed {}, which had been read",
                    path.display()
                ));
            }
            if access.updated && !access.created && now.as_ref().is_ok_and(|m| !m.is_dir()) {
                return Err(format!("it modified {}", path.display()));
            }
            match &now {
                Ok(meta) if meta.is_file() => {
                    if !in_out_dir(path) {
                        return Err(format!("it wrote {} outside OUT_DIR", path.display()));
                    }
                    if output_set.insert(place) {
                        outputs.push((root, rel.to_owned(), path.clone()));
                    }
                }
                Ok(meta) if meta.is_dir() => (),
                Ok(_) => return Err(format!("it created {}, not a file", path.display())),
                Err(_) => (),
            }
            continue;
        }
        if access.removed {
            if access.created || access.kind == Kind::Absent || was_made(&place) {
                continue;
            }
            return Err(format!("it removed {}", path.display()));
        }
        if access.created || was_made(&place) {
            continue;
        }
        let passed = access.kind == Kind::Dir && access.listing.is_none() && access.link.is_none();
        if passed && (leads_to(path) || in_out_dir(path) || out_dir.starts_with(path)) {
            continue;
        }
        let state = match access.kind {
            Kind::Absent if access.appeared || (now.is_ok() && fs::metadata(path).is_ok()) => {
                return Err(format!("{} appeared while it ran", path.display()));
            }
            Kind::Absent => State::Absent,
            Kind::Dir => {
                if let Some(names) = &access.listing {
                    listings.insert(place.clone(), names.clone());
                }
                State::Dir(None)
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

    // Merge inputs from nested cached compilers/tools, skipping anything made
    // internally by this build script or its tools.
    let mut have: HashSet<Place> = manifest
        .inputs
        .iter()
        .map(|i| (i.root, i.path.clone()))
        .collect();
    for (place, (link, state)) in child_inputs {
        if !was_made(&place) && have.insert(place.clone()) {
            manifest.inputs.push(Input {
                root: place.0,
                path: place.1,
                link,
                state,
            });
        }
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
        let place = (root, rel.to_owned());
        if have.insert(place) {
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
    }

    // Track ELF dynamic loaders / script interpreters of executed binaries.
    let mut depth = 0;
    while let Some(program) = executed.pop() {
        let Some(loader) = crate::scan::interpreter(&program) else {
            continue;
        };
        let (root, rel) = roots.split(&loader);
        let place = (root, rel.to_owned());
        if !have.insert(place) {
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

    // Record PATH lookups (both direct and from nested tools).
    let path_list = path_dirs(&req.cwd, &req.env);
    let mut names = BTreeSet::new();
    for path in report.paths.keys() {
        if let (Some(dir), Some(name)) = (path.parent(), path.file_name())
            && path_list.iter().any(|d| d == dir)
        {
            names.insert(name.to_owned());
        }
    }
    for name in child_which {
        names.insert(os(&name));
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

    // Record twins for files beside the workspace so checkout-adjacent files
    // cannot cross-contaminate.
    if let Some(work) = roots.path(roots::WORK) {
        let around = Surroundings::of(work, &req.env);
        let twins: Vec<Input> = manifest
            .inputs
            .iter()
            .filter(|i| {
                i.root == roots::ABSOLUTE
                    && around.beside(&i.path)
                    && !around.named(&i.path)
                    && !around.kept(&i.path)
            })
            .map(|i| {
                let (up, rest) = around.above(&i.path);
                Input {
                    root: roots::WORK,
                    path: Surroundings::from_inside(up, rest),
                    link: i.link.clone(),
                    state: i.state.clone(),
                }
            })
            .collect();
        for twin in twins {
            if have.insert((twin.root, twin.path.clone())) {
                manifest.inputs.push(twin);
            }
        }
    }

    // Compute directory listing digests, filtering out this script's own outputs.
    let mut dir_own: HashMap<(u8, &Path), HashSet<&OsStr>> = HashMap::new();
    for (root, rel, _) in &outputs {
        if let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) {
            dir_own.entry((*root, parent)).or_default().insert(name);
        }
    }
    for input in &mut manifest.inputs {
        let place = (input.root, input.path.clone());
        if let (State::Dir(None), Some(mut entries)) = (&input.state, listings.remove(&place)) {
            let own_direct = dir_own.get(&(place.0, place.1.as_path()));
            entries.retain(|name| !own_direct.is_some_and(|set| set.contains(name.as_os_str())));
            input.state = State::Dir(Some(observe::listing_digest(entries)));
        }
    }

    // Stage all surviving OUT_DIR outputs.
    let to_read: Vec<&Path> = outputs.iter().map(|(_, _, p)| p.as_path()).collect();
    let mut staged: Vec<Staged> = Vec::with_capacity(to_read.len());
    let spellings: Vec<u8> = roots.spellings().iter().map(|(id, _)| *id).collect();
    for pin in child_pins {
        manifest.pins.push(pin);
    }
    for ((root, rel, _), staged_res) in outputs
        .iter()
        .zip(store.stage_many_with(&to_read, roots, false))
    {
        let out = staged_res.map_err(|e| format!("{e:#}"))?;
        for (id, named) in spellings.iter().zip(&out.names) {
            let pin = (*id, roots.path(*id).unwrap().to_owned());
            if *named && !manifest.pins.contains(&pin) {
                manifest.pins.push(pin);
            }
        }
        manifest.outputs.push(Output {
            root: *root,
            path: rel.clone(),
            digest: out.digest,
            size: out.size,
            mode: out.mode,
            encoding: out.encoding,
        });
        staged.push(out);
    }

    let detail = encode_detail(0, &roots.mark(stdout), &roots.mark(stderr));
    manifest.detail = *blake3::hash(&detail).as_bytes();
    manifest.normalize();
    match store.commit(recipe, &manifest, &detail, ran.millis, &mut staged) {
        Ok(Some(_)) => {
            store.count("script_stored", 1);
            log(
                &script_label(req),
                format_args!(
                    "stored: ran {} ms, {} inputs, {} outputs",
                    ran.millis,
                    manifest.inputs.len(),
                    manifest.outputs.len(),
                ),
            );
            Ok(manifest)
        }
        Ok(None) => Err("larger than half the cache".into()),
        Err(e) => Err(format!("{e:#}")),
    }
}

// ---- wrapper side (`jato __script <exe> [args...]`) -----------------------

fn direct_command(binary: &OsStr, rest: &[OsString], strip_session: bool) -> Command {
    let mut cmd = Command::new(binary);
    cmd.args(rest);
    cmd.env("JATO_SCRIPT_DIRECT", "1");
    if strip_session {
        cmd.env_remove("JATO_SESSION");
        if let (Some(dir), Some(path)) = (env::var_os(crate::tools::DIRECTORY), env::var_os("PATH"))
        {
            cmd.env("PATH", crate::tools::without(&path, &dir));
        }
        cmd.env_remove(crate::tools::DIRECTORY);
    }
    if let Some(preload) = env::var_os("LD_PRELOAD") {
        let kept: Vec<&[u8]> = preload
            .as_bytes()
            .split(|b| *b == b':' || *b == b' ')
            .filter(|entry| !entry.is_empty())
            .filter(|entry| !observe::is_script_library(Path::new(OsStr::from_bytes(entry))))
            .collect();
        if kept.is_empty() {
            cmd.env_remove("LD_PRELOAD");
        } else {
            cmd.env("LD_PRELOAD", OsString::from_vec(kept.join(&b':')));
        }
    }
    cmd
}

fn run_script_piped(
    binary: &OsStr,
    rest: &[OsString],
    observed: Option<observe::Config>,
    strip_session: bool,
) -> Result<(Ran, Report)> {
    let begun = Instant::now();
    let mut cmd = direct_command(binary, rest, strip_session);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let (mut child, session) = match observed {
        Some(config) => {
            let (child, session) = observe::spawn(&mut cmd, config)?;
            (child, Some(session))
        }
        None => (cmd.spawn()?, None),
    };
    drop(cmd);
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let copy_out = std::thread::spawn(move || tee(out, io::stdout()));
    let (stderr, streamed_err) = tee(err, io::stderr());
    let (stdout, streamed_out) = copy_out
        .join()
        .unwrap_or((None, Err(io::Error::other("output thread failed"))));
    if streamed_err.and(streamed_out).is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let report = session.map(observe::Session::finish).unwrap_or_default();
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

pub fn runner(args: &[OsString]) -> Result<i32> {
    let Some((binary, rest)) = args.split_first() else {
        anyhow::bail!("missing build-script binary");
    };
    let direct = || command::passthrough(direct_command(binary, rest, false));
    let Some(session) = env::var_os("JATO_SESSION") else {
        return direct();
    };
    let Ok(mut stream) = address(&session).and_then(|addr| UnixStream::connect_addr(&addr)) else {
        return direct();
    };
    let mut w = Writer(vec![REQ_SCRIPT]);
    w.number(std::process::id().into());
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
        HIT => {
            let code = r.number()? as i32;
            io::stdout().write_all(r.bytes()?)?;
            io::stderr().write_all(r.bytes()?)?;
            Ok(code)
        }
        RUN => {
            let (ran, _) = run_script_piped(binary, rest, None, false)?;
            let mut w = Writer(Vec::new());
            w.number(ran.code as u32 as u64);
            w.0.push(ran.success.into());
            w.optional(ran.stdout.as_deref());
            w.optional(ran.stderr.as_deref());
            w.number(ran.millis);
            if wire::send(&mut stream, &w.0).is_ok() {
                let _ = wire::receive(&mut stream);
            }
            Ok(ran.code)
        }
        RUN_OWN => {
            let store_dir = PathBuf::from(os(r.bytes()?));
            let mut ignore = Vec::new();
            for _ in 0..r.count()? {
                ignore.push(PathBuf::from(os(r.bytes()?)));
            }
            let config = observe::Config {
                fast: observe::fast_library(&store_dir),
                ignore,
                unlisted: Vec::new(),
                state: None,
                programs_in: None,
                mount: None,
            };
            let (mut ran, mut report) = match run_script_piped(binary, rest, Some(config), true) {
                Ok(done) => done,
                Err(_) => return direct(),
            };
            if !ran.success
                && report
                    .incomplete
                    .as_deref()
                    .is_some_and(|why| why.contains("system call listener"))
            {
                let (direct_ran, _) = run_script_piped(binary, rest, None, true)?;
                ran = direct_ran;
                report.incomplete = Some("required its own system call listener".into());
            }
            let mut w = Writer(Vec::new());
            w.number(ran.code as u32 as u64);
            w.0.push(ran.success.into());
            w.optional(ran.stdout.as_deref());
            w.optional(ran.stderr.as_deref());
            w.number(ran.millis);
            report.encode(&mut w);
            if wire::send(&mut stream, &w.0).is_ok() {
                let _ = wire::receive(&mut stream);
            }
            Ok(ran.code)
        }
        DECLINE => direct(),
        _ => direct(),
    }
}
