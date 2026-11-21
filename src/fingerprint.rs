use crate::invocation::Invocation;
use crate::store::{Digest, Store};
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn field(hash: &mut blake3::Hasher, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn stamp(meta: &fs::Metadata) -> [u64; 6] {
    [
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.ctime() as u64,
        meta.ctime_nsec() as u64,
        meta.mtime_nsec() as u64,
    ]
}

fn stamp_path(hash: &mut blake3::Hasher, path: &Path) -> Result<()> {
    field(hash, path.as_os_str().as_bytes());
    for n in stamp(&fs::metadata(path)?) {
        hash.update(&n.to_le_bytes());
    }
    Ok(())
}

fn ordered(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = fs::read_dir(dir)?
        .map(|e| e.map(|v| v.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.sort();
    Ok(paths)
}

pub fn recipe(compiler: &OsStr, args: &[OsString]) -> Result<Digest> {
    let cwd = env::current_dir()?;
    let bin = Path::new(compiler);
    let root = bin
        .parent()
        .and_then(Path::parent)
        .filter(|p| p.is_absolute() && p.join("lib/rustlib").is_dir());
    let root = match root {
        Some(path) => path.to_owned(),
        None => {
            let output = Command::new(compiler)
                .args(["--print", "sysroot"])
                .output()?;
            ensure!(output.status.success(), "cannot identify sysroot");
            PathBuf::from(std::str::from_utf8(&output.stdout)?.trim_end())
        }
    };
    let mut hash = blake3::Hasher::new_derive_key("jato local rustc recipe v1");
    field(&mut hash, compiler.as_bytes());
    field(&mut hash, cwd.as_os_str().as_bytes());
    for arg in args {
        field(&mut hash, arg.as_bytes());
    }
    stamp_path(&mut hash, &root.join("bin/rustc"))?;
    for p in ordered(&root.join("lib"))? {
        if p.is_file() {
            stamp_path(&mut hash, &p)?;
        }
    }
    for target in ordered(&root.join("lib/rustlib"))? {
        let dir = target.join("lib");
        if dir.is_dir() {
            for p in ordered(&dir)? {
                stamp_path(&mut hash, &p)?;
            }
        }
    }
    let mut settings: Vec<_> = env::vars_os()
        .filter(|(k, _)| {
            let k = k.as_bytes();
            k.starts_with(b"RUST")
                || k.starts_with(b"LC_")
                || matches!(
                    k,
                    b"LANG"
                        | b"LANGUAGE"
                        | b"LD_LIBRARY_PATH"
                        | b"DYLD_LIBRARY_PATH"
                        | b"SOURCE_DATE_EPOCH"
                        | b"PATH"
                )
        })
        .collect();
    settings.sort();
    for (k, v) in settings {
        field(&mut hash, k.as_bytes());
        field(&mut hash, v.as_bytes());
    }
    Ok(*hash.finalize().as_bytes())
}

fn recent(m: &fs::Metadata, since: SystemTime) -> bool {
    let epoch = since.duration_since(UNIX_EPOCH).unwrap_or_default();
    (m.ctime(), m.ctime_nsec()) >= (epoch.as_secs() as i64, epoch.subsec_nanos() as i64)
        || m.modified().is_ok_and(|t| t >= since)
}

fn hash_file(path: &Path, since: SystemTime) -> Result<Digest> {
    let mut f = File::open(path)?;
    let before = f.metadata()?;
    ensure!(
        before.is_file() && !recent(&before, since),
        "input changed during compilation"
    );
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(&mut f)?;
    ensure!(
        stamp(&before) == stamp(&fs::metadata(path)?),
        "input changed while hashing"
    );
    Ok(*hasher.finalize().as_bytes())
}

struct Dependencies {
    files: BTreeSet<PathBuf>,
    env: BTreeSet<String>,
}

fn dependencies(path: &Path) -> Result<Dependencies> {
    // rustc emits one empty Make rule per input. Their entire left-hand sides
    // are filenames, including escaped spaces. Cross-check with the first
    // output rule so an unfamiliar dep-info dialect fails closed.
    const LIMIT: u64 = 8 << 20;
    let f = File::open(path)?;
    ensure!(f.metadata()?.len() <= LIMIT, "dep-info exceeds size limit");
    let mut files = BTreeSet::new();
    let mut env = BTreeSet::new();
    let mut first = None;
    for line in BufReader::new(f.take(LIMIT + 1)).lines() {
        let line = line?;
        if let Some(value) = line.strip_prefix("# env-dep:") {
            env.insert(value.to_owned());
        } else if let Some(file) = line.strip_suffix(':') {
            ensure!(
                !file.is_empty() && !file.contains(['\r', '\t']),
                "unrecognized dep-info path"
            );
            files.insert(file.to_owned());
        } else if !line.is_empty() && first.is_none() {
            first = Some(line);
        }
    }
    let rule = first.context("missing dep-info rule")?;
    let (_, list) = rule
        .split_once(": ")
        .context("unrecognized dep-info rule")?;
    // Do not split on whitespace: even output directories can contain spaces.
    let mut encoded = BTreeSet::new();
    let mut current = String::new();
    let mut chars = list.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&' ') {
            current.push(c);
            current.push(chars.next().unwrap());
        } else if c == ' ' {
            if !current.is_empty() {
                encoded.insert(std::mem::take(&mut current));
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        encoded.insert(current);
    }
    ensure!(
        encoded == files && !files.is_empty(),
        "incomplete dep-info input list"
    );
    Ok(Dependencies {
        files: files
            .into_iter()
            .map(|s| PathBuf::from(s.replace("\\ ", " ")))
            .collect(),
        env,
    })
}

pub fn key(
    recipe: &Digest,
    inv: &Invocation,
    dep_info: &Path,
    since: SystemTime,
    externs: &Digest,
) -> Result<Digest> {
    let deps = dependencies(dep_info)?;
    let root = fs::canonicalize(&inv.source)?;
    let mut has_root = false;
    let mut hash = blake3::Hasher::new_derive_key("jato verified Rust inputs v1");
    field(&mut hash, recipe);
    for file in deps.files {
        let path =
            fs::canonicalize(&file).with_context(|| format!("resolving {}", file.display()))?;
        has_root |= root == path;
        field(&mut hash, b"source");
        field(&mut hash, file.as_os_str().as_bytes());
        field(&mut hash, path.as_os_str().as_bytes());
        field(&mut hash, &hash_file(&path, since)?);
    }
    ensure!(has_root, "crate root missing from dep-info");
    for env in deps.env {
        field(&mut hash, b"env");
        field(&mut hash, env.as_bytes());
    }
    field(&mut hash, externs);
    Ok(*hash.finalize().as_bytes())
}

pub fn extern_key(inv: &Invocation, since: SystemTime, store: &Store) -> Result<Digest> {
    let mut hash = blake3::Hasher::new_derive_key("jato extern closure v1");
    for path in &inv.externs {
        let digest = hash_file(path, since)?;
        // A pure-looking rlib can re-export a procedural macro. Trust only a
        // dependency chain whose outputs Jato previously admitted itself.
        if !store.knows_artifact(&digest)? {
            bail!("unverified extern {}", path.display());
        }
        field(&mut hash, b"extern");
        field(&mut hash, &digest);
    }
    Ok(*hash.finalize().as_bytes())
}
