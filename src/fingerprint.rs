mod toolchain;

use crate::invocation::Invocation;
use crate::store::{Digest, Store};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn field(hash: &mut blake3::Hasher, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

pub(crate) fn stamp(meta: &fs::Metadata) -> [u64; 7] {
    [
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.ctime() as u64,
        meta.ctime_nsec() as u64,
        meta.mtime() as u64,
        meta.mtime_nsec() as u64,
    ]
}

pub struct Recipe {
    pub key: Digest,
    pub toolchain: Digest,
    pub root: PathBuf,
}

pub fn recipe(
    compiler: &OsStr,
    args: &[OsString],
    native: bool,
    store: &mut Store,
) -> Result<Recipe> {
    let cwd = env::current_dir()?;
    let (root, toolchain) = toolchain::resolve(compiler, &cwd, store)?;
    let mut hash = blake3::Hasher::new_derive_key("jato local rustc recipe v9");
    if env::var_os("JATO_WORKTREE_ACTIVE").as_deref() == Some(OsStr::new("1")) {
        field(&mut hash, b"jato worktree paths v1");
    }
    field(&mut hash, &toolchain);
    field(&mut hash, compiler.as_bytes());
    field(&mut hash, cwd.as_os_str().as_bytes());
    for arg in args {
        field(&mut hash, arg.as_bytes());
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
    if let Ok(status) = fs::read_to_string("/proc/self/status")
        && let Some(mask) = status.lines().find(|l| l.starts_with("Umask:"))
    {
        field(&mut hash, mask.as_bytes());
    }
    if native {
        field(&mut hash, b"native manifest v2");
        crate::native::environment(&mut hash);
    }
    Ok(Recipe {
        root,
        key: *hash.finalize().as_bytes(),
        toolchain,
    })
}

pub(crate) fn recent(m: &fs::Metadata, since: SystemTime) -> bool {
    let epoch = since.duration_since(UNIX_EPOCH).unwrap_or_default();
    (m.ctime(), m.ctime_nsec()) >= (epoch.as_secs() as i64, epoch.subsec_nanos() as i64)
        || m.modified().is_ok_and(|t| t >= since)
}

fn hash_open(
    mut f: File,
    path: &Path,
    since: SystemTime,
    scan: Option<&mut Vec<u8>>,
    inv: &Invocation,
    observed: bool,
    nightly_guards: Option<&mut Vec<String>>,
) -> Result<(Digest, u64)> {
    let before = f.metadata()?;
    ensure!(
        before.is_file() && !recent(&before, since),
        "input changed during compilation"
    );
    let digest = if let Some(buffer) = scan {
        const MAX: u64 = 8 << 20;
        if before.len() > MAX {
            ensure!(nightly_guards.is_none(), "oversized nightly crate root");
            crate::admission::large_input(f.take(before.len() + 1), path)?
        } else {
            buffer.clear();
            f.take(before.len() + 1).read_to_end(buffer)?;
            ensure!(
                buffer.len() as u64 == before.len(),
                "source changed size during admission"
            );
            if let Some(guards) = nightly_guards {
                crate::admission::check_root(buffer, guards)?;
            }
            if inv.codegen {
                crate::admission::verify_bytes(buffer, path, inv.deferred_links, observed)?;
            }
            blake3::hash(buffer)
        }
    } else if before.len() >= 4 << 20 {
        let input = crate::content::Input::with_file(path, f, since)?;
        return Ok((crate::content::digests(&[input], 2 << 20)?[0], before.len()));
    } else {
        let mut h = blake3::Hasher::new();
        h.update_reader(&mut f)?;
        h.finalize()
    };
    ensure!(
        stamp(&before) == stamp(&fs::metadata(path)?),
        "input changed while hashing"
    );
    Ok((*digest.as_bytes(), before.len()))
}

#[derive(Serialize, Deserialize)]
pub struct Plan {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub nightly: bool,
    files: BTreeSet<PathBuf>,
    env: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub large_input: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub large_extern: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<crate::native::Manifest>,
}

pub fn dependencies(path: &Path) -> Result<Plan> {
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
            let (name, recorded) = value
                .split_once('=')
                .map_or((value, None), |(k, v)| (k, Some(v)));
            // Unusual escaped variable names still compile through the normal path.
            ensure!(
                !name.is_empty() && !name.contains(['\\', '\0']),
                "unsupported environment name"
            );
            let actual = std::env::var(name).ok().map(|v| {
                v.replace('\\', "\\\\")
                    .replace('\n', "\\n")
                    .replace('\r', "\\r")
            });
            ensure!(
                actual.as_deref() == recorded,
                "compiler supplied a synthetic environment value"
            );
            env.insert(name.to_owned());
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
    Ok(Plan {
        nightly: false,
        files: files
            .into_iter()
            .map(|s| PathBuf::from(s.replace("\\ ", " ")))
            .collect(),
        env,
        native: None,
        large_input: None,
        large_extern: None,
    })
}

pub struct Inputs {
    pub nightly_guards: Vec<String>,
    pub sources: Digest,
    pub large_input: Option<usize>,
    namespace: BTreeSet<PathBuf>,
    covered: BTreeSet<PathBuf>,
}

fn tracked_env(hash: &mut blake3::Hasher, names: &BTreeSet<String>) {
    for name in names {
        field(hash, b"env");
        field(hash, name.as_bytes());
        match env::var_os(name) {
            Some(value) => {
                field(hash, b"set");
                field(hash, value.as_bytes());
            }
            None => field(hash, b"unset"),
        }
    }
}

fn sample_file(
    h: &mut blake3::Hasher,
    path: &Path,
    since: SystemTime,
    buffer: &mut [u8],
) -> Result<()> {
    use std::os::unix::fs::FileExt;
    let file = crate::content::open_with_flags(path, libc::O_NONBLOCK)?;
    let m = file.metadata()?;
    ensure!(
        m.is_file() && !recent(&m, since),
        "sampled input changed during compilation"
    );
    field(h, path.as_os_str().as_bytes());
    h.update(&m.len().to_le_bytes());
    let width = m.len().min(buffer.len() as u64);
    let tail = m.len() - width;
    let mut previous = None;
    for offset in [0, tail / 2, tail] {
        if previous != Some(offset) {
            file.read_exact_at(&mut buffer[..width as usize], offset)?;
            h.update(&buffer[..width as usize]);
        }
        previous = Some(offset);
    }
    ensure!(
        stamp(&m) == stamp(&fs::metadata(path)?),
        "sampled input replaced during lookup"
    );
    Ok(())
}

impl Plan {
    pub fn same_rust_inputs(&self, other: &Self) -> bool {
        self.files == other.files && self.env == other.env
    }

    // A sample can only reject a candidate. Even a perfect match goes
    // through the original whole-file hashes, extern proof and native checks.
    pub fn sample(
        &self,
        recipe: &Recipe,
        inv: &Invocation,
        since: SystemTime,
    ) -> Result<Option<Digest>> {
        if self.large_input.is_none() && self.large_extern.is_none() {
            return Ok(None);
        }
        let mut h = blake3::Hasher::new_derive_key("jato bulk input precheck v1");
        field(&mut h, &recipe.key);
        let mut buffer = [0; 1024];
        sample_file(&mut h, &inv.source, since, &mut buffer)?;
        if let Some(index) = self.large_input {
            let large = self
                .files
                .iter()
                .nth(index)
                .context("invalid large input index")?;
            if *large != inv.source {
                sample_file(&mut h, large, since, &mut buffer)?;
            }
            for path in self
                .files
                .iter()
                .filter(|p| p.extension() == Some(OsStr::new("rs")))
                .take(8)
            {
                if *path != inv.source && path != large {
                    sample_file(&mut h, path, since, &mut buffer)?;
                }
            }
        }
        tracked_env(&mut h, &self.env);
        if let Some(index) = self.large_extern {
            field(&mut h, b"compiled dependency precheck v1");
            let large = inv
                .externs
                .get(index)
                .context("invalid large extern index")?;
            sample_file(&mut h, large, since, &mut buffer)?;
            for p in inv.externs.iter().take(8).filter(|p| *p != large) {
                sample_file(&mut h, p, since, &mut buffer)?;
            }
        }
        Ok(Some(*h.finalize().as_bytes()))
    }

    pub fn inputs(
        &self,
        recipe: &Recipe,
        inv: &Invocation,
        since: SystemTime,
        recording: bool,
    ) -> Result<Inputs> {
        let root = fs::canonicalize(&inv.source)?;
        let mut has_root = false;
        let mut hash = blake3::Hasher::new_derive_key("jato verified Rust inputs v2");
        let mut namespace = BTreeSet::new();
        field(&mut hash, &recipe.key);
        let mut covered = BTreeSet::new();
        let mut ancestors = BTreeSet::new();
        let mut scan = Vec::new();
        let mut nightly_guards = Vec::new();
        let cwd = std::env::current_dir()?;
        let mut largest = None;
        let mut largest_size = 0;
        for (index, file) in self.files.iter().enumerate() {
            let opened = crate::content::open(file)
                .with_context(|| format!("opening {}", file.display()))?;
            #[cfg(target_os = "linux")]
            let path = fs::read_link(format!("/proc/self/fd/{}", opened.as_raw_fd()))
                .or_else(|_| fs::canonicalize(file))?;
            #[cfg(not(target_os = "linux"))]
            let path = fs::canonicalize(file)?;
            if self.native.is_some() {
                ensure!(
                    !["/proc", "/sys", "/dev"]
                        .iter()
                        .any(|p| path.starts_with(p)),
                    "volatile compiler input"
                );
                covered.insert(path.clone());
            }
            has_root |= root == path;
            field(&mut hash, b"source");
            field(&mut hash, file.as_os_str().as_bytes());
            field(&mut hash, path.as_os_str().as_bytes());
            let nightly_root = recording && self.nightly && root == path;
            let (digest, length) = hash_open(
                opened,
                file,
                since,
                (recording && (inv.codegen || nightly_root)).then_some(&mut scan),
                inv,
                self.native.is_some(),
                nightly_root.then_some(&mut nightly_guards),
            )?;
            field(&mut hash, &digest);
            if length >= 4 << 20 && length > largest_size {
                largest_size = length;
                largest = Some(index);
            }
            // rustc can change module resolution when a second legal location
            // appears. Track the containing directories and the alternate name,
            // even when the alternate directory or file does not yet exist.
            let original = cwd.join(file);
            for resolved in std::iter::once(&path).chain((original != path).then_some(&original)) {
                let parent = resolved.parent().context("source without a parent")?;
                namespace.insert(parent.to_owned());
                if resolved.file_name() == Some(OsStr::new("mod.rs")) {
                    namespace.insert(parent.with_extension("rs"));
                } else if resolved.extension() == Some(OsStr::new("rs")) {
                    namespace.insert(resolved.with_extension("").join("mod.rs"));
                }
            }
            if original != path {
                for component in original.ancestors() {
                    if !ancestors.insert(component.to_owned()) {
                        break;
                    }
                }
            }
        }
        for p in ancestors {
            let m = fs::symlink_metadata(p)?;
            ensure!(
                !m.is_symlink() || !recent(&m, since),
                "input symlink changed during compilation"
            );
        }
        ensure!(has_root, "crate root missing from dep-info");
        tracked_env(&mut hash, &self.env);
        Ok(Inputs {
            nightly_guards,
            sources: *hash.finalize().as_bytes(),
            large_input: largest,
            namespace,
            covered,
        })
    }
}

impl Inputs {
    pub fn key(
        mut self,
        plan: &Plan,
        recipe: &Recipe,
        inv: &Invocation,
        since: SystemTime,
        externs: &Digest,
    ) -> Result<Digest> {
        let mut hash = blake3::Hasher::new_derive_key("jato verified compilation v3");
        if plan.nightly {
            field(&mut hash, b"jato restricted nightly inputs v1");
        }
        field(&mut hash, &self.sources);
        field(&mut hash, externs);
        if let Some(native) = &plan.native {
            for p in &inv.externs {
                self.covered.insert(fs::canonicalize(p)?);
            }
            field(
                &mut hash,
                &native.digest(&recipe.root, since, &self.covered, &inv.outputs)?,
            );
        }
        Ok(*hash.finalize().as_bytes())
    }

    pub fn topology(&self, since: SystemTime) -> Result<Digest> {
        let mut topology = blake3::Hasher::new();
        for p in &self.namespace {
            field(&mut topology, p.as_os_str().as_bytes());
            match fs::metadata(p) {
                Ok(m) => {
                    ensure!(
                        !recent(&m, since),
                        "module search path changed during compilation"
                    );
                    for n in stamp(&m) {
                        topology.update(&n.to_le_bytes());
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    field(&mut topology, b"absent");
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(*topology.finalize().as_bytes())
    }

    // Editors commonly replace a tracked file by rename. Its directory's
    // timestamps change even if rustc sees the same names and file contents.
    // Keep an independent witness only for small, ordinary source trees.
    pub fn editor_topology(&self, since: SystemTime) -> Result<Digest> {
        ensure!(self.namespace.len() <= 256, "large module search graph");
        let mut h = blake3::Hasher::new_derive_key("jato local editor directory layout v1");
        let mut remaining = 512usize;
        for path in &self.namespace {
            field(&mut h, path.as_os_str().as_bytes());
            let m = match fs::symlink_metadata(path) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    field(&mut h, b"absent");
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            ensure!(!recent(&m, since), "module candidate changed");
            if m.is_file() {
                field(&mut h, b"file");
                for word in stamp(&m) {
                    h.update(&word.to_le_bytes());
                }
                continue;
            }
            ensure!(
                m.is_dir() && fs::canonicalize(path)? == *path,
                "redirected module directory"
            );
            field(&mut h, b"directory");
            for n in [
                m.dev(),
                m.ino(),
                m.mode() as u64,
                m.uid() as u64,
                m.gid() as u64,
            ] {
                h.update(&n.to_le_bytes());
            }
            let mut names = Vec::new();
            for child in fs::read_dir(path)? {
                remaining = remaining.checked_sub(1).context("large module directory")?;
                names.push(child?.file_name());
            }
            names.sort_unstable();
            h.update(&(names.len() as u64).to_le_bytes());
            for name in names {
                let child = path.join(&name);
                let info = fs::symlink_metadata(&child)?;
                ensure!(
                    info.is_file() || info.is_dir(),
                    "special entry in module directory"
                );
                field(&mut h, name.as_bytes());
                h.update(&info.mode().to_le_bytes());
                h.update(&info.uid().to_le_bytes());
                h.update(&info.gid().to_le_bytes());
                if (info.is_file() && self.covered.contains(&child))
                    || (info.is_dir() && self.namespace.contains(&child))
                {
                    // The complete file content or directory layout is checked
                    // separately as part of this exact compiler entry.
                    field(&mut h, b"tracked");
                } else {
                    ensure!(!recent(&info, since), "untracked directory entry changed");
                    field(&mut h, b"other");
                    for n in stamp(&info) {
                        h.update(&n.to_le_bytes());
                    }
                }
            }
            ensure!(
                stamp(&m) == stamp(&fs::metadata(path)?),
                "module directory changed during enumeration"
            );
        }
        Ok(*h.finalize().as_bytes())
    }
}

pub struct Externs {
    pub key: Digest,
    pub largest: Option<usize>,
    pub needs: Vec<Digest>,
}

fn extern_inputs(inv: &Invocation, since: SystemTime) -> Result<Vec<crate::content::Input<'_>>> {
    inv.externs
        .iter()
        .map(|path| {
            if inv.externs.len() <= 16 {
                crate::content::Input::with_file(path, crate::content::open(path)?, since)
            } else {
                crate::content::Input::new(path, &fs::metadata(path)?, since)
            }
        })
        .collect()
}

// Preflight is a negative admission filter. A size match can start an
// observation, but only a full certified content digest can publish it.
pub fn native_externs<'a>(
    inv: &'a Invocation,
    since: SystemTime,
    store: &Store,
) -> Result<Vec<crate::content::Input<'a>>> {
    let inputs = extern_inputs(inv, since)?;
    for input in &inputs {
        ensure!(
            store.has_artifact_size(input.size(), inv.codegen)?,
            "unverified extern {}",
            input.path().display()
        );
    }
    Ok(inputs)
}

pub fn externs(inv: &Invocation, since: SystemTime, store: &Store, defer: bool) -> Result<Externs> {
    certify_externs(
        &extern_inputs(inv, since)?,
        inv.codegen,
        since,
        store,
        defer,
    )
}

pub fn certify_externs(
    inputs: &[crate::content::Input<'_>],
    codegen: bool,
    since: SystemTime,
    store: &Store,
    defer: bool,
) -> Result<Externs> {
    let mut hash = blake3::Hasher::new_derive_key("jato extern closure v1");
    let mut needs = Vec::new();
    // Small metadata reads are often on a busy, wide check graph. Give newly
    // ready Cargo units priority there; reserve finer stripes for codegen.
    let bytes_per_worker = if codegen { 2 << 20 } else { 8 << 20 };
    for (input, digest) in inputs
        .iter()
        .zip(crate::content::digests(inputs, bytes_per_worker)?)
    {
        let path = input.path();
        if !store.knows_artifact(&digest, codegen)? {
            if !defer
                || path.extension() != Some(OsStr::new("rmeta"))
                || since
                    .duration_since(fs::metadata(path)?.modified()?)
                    .unwrap_or_default()
                    .as_secs()
                    > 120
            {
                bail!("unverified extern {}", path.display());
            }
            needs.push(digest);
        }
        field(&mut hash, b"extern");
        field(&mut hash, &digest);
    }
    Ok(Externs {
        key: *hash.finalize().as_bytes(),
        largest: inputs
            .iter()
            .enumerate()
            .filter(|(_, input)| input.size() >= 4 << 20)
            .max_by_key(|(_, input)| input.size())
            .map(|(i, _)| i),
        needs,
    })
}
