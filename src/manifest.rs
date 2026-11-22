//! What a command's result depended on, and whether that still holds.
//!
//! A manifest lists every path the command examined with what was there:
//! a file's digest, a directory, the names in a directory, or nothing. It
//! also lists the environment variables, program lookups and directories the
//! result is tied to. A result is reused only when all of it holds again.
use crate::cas::Store;
use crate::observe::{Kind, Stamp, kind_of, listing_digest};
use crate::roots::Roots;
use crate::scan::Digest;
use crate::wire::{Reader, Writer};
use anyhow::{Result, bail, ensure};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Absent,
    /// A directory, and the digest of its entry names if they were read.
    Dir(Option<Digest>),
    File(Digest),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    pub root: u8,
    pub path: PathBuf,
    /// Where the path led, if it was itself a symbolic link.
    pub link: Option<Vec<u8>>,
    pub state: State,
}

/// A root and a path beneath it.
pub type Place = (u8, PathBuf);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    pub inputs: Vec<Input>,
    /// Environment variables the result depends on, with roots marked.
    pub env: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    /// Program names looked up on PATH, and where each was found.
    pub which: Vec<(Vec<u8>, Option<Place>)>,
    /// Roots whose location the outputs contain.
    pub pins: Vec<(u8, PathBuf)>,
    /// The terminal width, if the diagnostics were laid out for one.
    pub width: Option<Option<u16>>,
    /// Whether the diagnostics were coloured, if any were rendered.
    pub ansi: Option<bool>,
    /// Files the command wrote. They do not count as entries of the
    /// directories it listed.
    pub outputs: Vec<Output>,
    /// Files from before the command that it removed.
    pub removed: Vec<Place>,
    /// Digest of what the command printed and its exit status.
    pub detail: Digest,
    /// What the compiler saved for compiling this crate again after a
    /// change. It is no part of the result: a later compilation that finds
    /// no such state of its own is given this to start from.
    pub saved: Vec<Output>,
    /// Identifies the workspace and target directory the state was saved
    /// in, where the compiler accepts it as it is.
    pub saved_in: Digest,
    /// Identifies the output directory as the compiler was given it. With
    /// any other, the compiler takes its analysis from the state and
    /// generates all code again.
    pub saved_for: Digest,
}

/// A file a command wrote, and the object that holds its bytes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Output {
    pub root: u8,
    pub path: PathBuf,
    pub digest: Digest,
    pub size: u64,
    pub mode: u32,
    /// The object is text in which marks stand for the roots.
    pub marked: bool,
}

const MAX_MANIFEST: usize = 64 << 20;

impl Manifest {
    /// Put the lists in the one order that identifies their contents.
    pub fn normalize(&mut self) {
        self.inputs
            .sort_by(|a, b| (a.root, &a.path).cmp(&(b.root, &b.path)));
        self.inputs
            .dedup_by(|a, b| (a.root, &a.path) == (b.root, &b.path));
        self.env.sort();
        self.env.dedup_by(|a, b| a.0 == b.0);
        self.which.sort();
        self.which.dedup_by(|a, b| a.0 == b.0);
        self.pins.sort();
        self.outputs.sort();
        self.outputs.dedup();
        self.removed.sort();
        self.removed.dedup();
    }

    fn conditions(&self) -> Vec<u8> {
        let mut w = Writer(Vec::with_capacity(self.inputs.len() * 96));
        w.number(self.inputs.len() as u64);
        for input in &self.inputs {
            w.path(input.root, &input.path);
            w.optional(input.link.as_deref());
            match &input.state {
                State::Absent => w.0.push(0),
                State::Dir(None) => w.0.push(1),
                State::Dir(Some(names)) => {
                    w.0.push(2);
                    w.0.extend_from_slice(names)
                }
                State::File(digest) => {
                    w.0.push(3);
                    w.0.extend_from_slice(digest)
                }
            }
        }
        w.number(self.env.len() as u64);
        for (name, value) in &self.env {
            w.bytes(name);
            w.optional(value.as_deref());
        }
        w.number(self.which.len() as u64);
        for (name, found) in &self.which {
            w.bytes(name);
            match found {
                Some((root, path)) => {
                    w.0.push(1);
                    w.path(*root, path)
                }
                None => w.0.push(0),
            }
        }
        w.number(self.pins.len() as u64);
        for (root, path) in &self.pins {
            w.path(*root, path);
        }
        match self.width {
            None => w.0.push(0),
            Some(None) => w.0.push(1),
            Some(Some(width)) => {
                w.0.push(2);
                w.number(width.into())
            }
        }
        w.0.push(match self.ansi {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        });
        w.0
    }

    /// Identifies a command together with everything its result depends on.
    pub fn key(&self, recipe: &Digest) -> Digest {
        let mut hash = blake3::Hasher::new_derive_key("jato command result v1");
        hash.update(recipe);
        hash.update(&self.conditions());
        *hash.finalize().as_bytes()
    }

    /// The stored form. It carries a checksum, and everything needed to
    /// restore the result, so a damaged index can hide a result but cannot
    /// make one stand in for another.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut w = Writer(self.conditions());
        w.number(self.outputs.len() as u64);
        for output in &self.outputs {
            w.path(output.root, &output.path);
            w.0.extend_from_slice(&output.digest);
            w.number(output.size);
            w.number(output.mode.into());
            w.0.push(output.marked.into());
        }
        w.number(self.removed.len() as u64);
        for (root, path) in &self.removed {
            w.path(*root, path);
        }
        w.0.extend_from_slice(&self.detail);
        w.number(self.saved.len() as u64);
        for file in &self.saved {
            w.path(file.root, &file.path);
            w.0.extend_from_slice(&file.digest);
            w.number(file.size);
        }
        w.0.extend_from_slice(&self.saved_in);
        w.0.extend_from_slice(&self.saved_for);
        ensure!(w.0.len() <= MAX_MANIFEST, "too many inputs to record");
        let mut packer = zstd::bulk::Compressor::new(1)?;
        packer.include_checksum(true)?;
        Ok(packer.compress(&w.0)?)
    }

    pub fn decode(stored: &[u8]) -> Result<Self> {
        let raw = zstd::bulk::decompress(stored, MAX_MANIFEST)?;
        let mut r = Reader(&raw);
        let mut m = Manifest::default();
        for _ in 0..r.count()? {
            let (root, path) = r.path()?;
            let link = r.optional()?.map(<[u8]>::to_vec);
            let state = match r.byte()? {
                0 => State::Absent,
                1 => State::Dir(None),
                2 => State::Dir(Some(r.digest()?)),
                3 => State::File(r.digest()?),
                _ => bail!("unknown manifest state"),
            };
            m.inputs.push(Input {
                root,
                path,
                link,
                state,
            });
        }
        for _ in 0..r.count()? {
            let name = r.bytes()?.to_vec();
            m.env.push((name, r.optional()?.map(<[u8]>::to_vec)));
        }
        for _ in 0..r.count()? {
            let name = r.bytes()?.to_vec();
            let found = match r.byte()? {
                0 => None,
                _ => Some(r.path()?),
            };
            m.which.push((name, found));
        }
        for _ in 0..r.count()? {
            m.pins.push(r.path()?);
        }
        m.width = match r.byte()? {
            0 => None,
            1 => Some(None),
            _ => Some(Some(u16::try_from(r.number()?)?)),
        };
        m.ansi = match r.byte()? {
            0 => None,
            1 => Some(false),
            _ => Some(true),
        };
        for _ in 0..r.count()? {
            let (root, path) = r.path()?;
            m.outputs.push(Output {
                root,
                path,
                digest: r.digest()?,
                size: r.number()?,
                mode: u32::try_from(r.number()?)?,
                marked: r.byte()? != 0,
            });
        }
        for _ in 0..r.count()? {
            m.removed.push(r.path()?);
        }
        m.detail = r.digest()?;
        for _ in 0..r.count()? {
            let (root, path) = r.path()?;
            m.saved.push(Output {
                root,
                path,
                digest: r.digest()?,
                size: r.number()?,
                mode: 0o644,
                marked: false,
            });
        }
        m.saved_in = r.digest()?;
        m.saved_for = r.digest()?;
        ensure!(r.0.is_empty(), "trailing manifest bytes");
        Ok(m)
    }
}

/// The first program called `name` that the directories of PATH offer.
pub fn which(name: &OsStr, dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter().map(|dir| dir.join(name)).find(|path| {
        path.is_file()
            && std::ffi::CString::new(path.as_os_str().as_bytes())
                .is_ok_and(|c| unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0)
    })
}

/// A command's environment.
pub type Env = std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>;

pub fn path_dirs(cwd: &Path, env: &Env) -> Vec<PathBuf> {
    env.get(OsStr::new("PATH"))
        .map(|path| {
            std::env::split_paths(path)
                .map(|dir| cwd.join(dir))
                .collect()
        })
        .unwrap_or_default()
}

/// The situation a manifest is checked against.
pub struct Here<'a> {
    pub roots: &'a Roots,
    pub env: &'a Env,
    pub path: &'a [PathBuf],
    pub width: Option<u16>,
    pub ansi: bool,
    /// What each path was found to be, kept across the candidates of one
    /// lookup so no file is examined twice.
    seen: HashMap<PathBuf, Seen>,
}

#[derive(Clone)]
struct Seen {
    link: Option<Vec<u8>>,
    kind: Kind,
    stamp: Option<Stamp>,
    digest: Option<Digest>,
}

impl<'a> Here<'a> {
    pub fn new(
        roots: &'a Roots,
        env: &'a Env,
        path: &'a [PathBuf],
        width: Option<u16>,
        ansi: bool,
    ) -> Self {
        Self {
            roots,
            env,
            path,
            width,
            ansi,
            seen: HashMap::new(),
        }
    }

    fn examine(&mut self, path: &Path) -> io::Result<Seen> {
        if let Some(seen) = self.seen.get(path) {
            return Ok(seen.clone());
        }
        let absent = |e: &io::Error| {
            matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            )
        };
        let seen = match fs::symlink_metadata(path) {
            Err(e) if absent(&e) => Seen {
                link: None,
                kind: Kind::Absent,
                stamp: None,
                digest: None,
            },
            Err(e) => return Err(e),
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = fs::read_link(path)?;
                let link = Some(self.roots.mark_os(target.as_os_str()).into_owned());
                match fs::metadata(path) {
                    Ok(meta) => Seen {
                        link,
                        kind: kind_of(&meta),
                        stamp: Some(Stamp::of(&meta)),
                        digest: None,
                    },
                    Err(e) if absent(&e) => Seen {
                        link,
                        kind: Kind::Absent,
                        stamp: None,
                        digest: None,
                    },
                    Err(e) => return Err(e),
                }
            }
            Ok(meta) => Seen {
                link: None,
                kind: kind_of(&meta),
                stamp: Some(Stamp::of(&meta)),
                digest: None,
            },
        };
        self.seen.insert(path.to_owned(), seen.clone());
        Ok(seen)
    }
}

/// The names in a directory that say nothing about what a command found
/// there: those its own outputs are beneath, and the one that leads to the
/// target directory, which is there or not depending on what was built
/// before. `dir` is the directory as a root and a path beneath it, `path`
/// the same directory in full.
pub fn own_names<'a>(
    dir: (u8, &Path),
    path: &Path,
    outputs: impl Iterator<Item = (u8, &'a Path)>,
    roots: &Roots,
) -> Vec<OsString> {
    let first = |rest: &Path| rest.components().next().map(|c| c.as_os_str().to_owned());
    let mut own: Vec<OsString> = outputs
        .filter(|(root, _)| *root == dir.0)
        .filter_map(|(_, output)| first(output.strip_prefix(dir.1).ok()?))
        .collect();
    let target = roots.path(crate::roots::TARGET);
    own.extend(target.and_then(|target| first(target.strip_prefix(path).ok()?)));
    own.sort();
    own.dedup();
    own
}

/// The names in a directory, without those in `own`.
pub fn list(dir: &Path, own: &[OsString]) -> io::Result<Digest> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name();
        if !own.contains(&name) {
            names.push(name);
        }
    }
    Ok(listing_digest(names))
}

impl Manifest {
    /// Whether everything this manifest records holds here. An error says
    /// what does not.
    pub fn holds(&self, here: &mut Here<'_>, store: &mut Store) -> Result<(), String> {
        for (root, recorded) in &self.pins {
            if here.roots.path(*root) != Some(recorded.as_path()) {
                return Err(format!(
                    "its output names {}, which is not where this build is",
                    recorded.display()
                ));
            }
        }
        if let Some(width) = self.width
            && width != here.width
        {
            return Err("its diagnostics were laid out for another terminal width".into());
        }
        if let Some(ansi) = self.ansi
            && ansi != here.ansi
        {
            return Err("its diagnostics were rendered with different colour settings".into());
        }
        for (name, recorded) in &self.env {
            let name = OsStr::from_bytes(name);
            let current = here.env.get(name).map(|v| here.roots.mark_os(v));
            if current.as_deref() != recorded.as_deref() {
                return Err(format!(
                    "the environment variable {} changed",
                    name.to_string_lossy()
                ));
            }
        }
        for (name, recorded) in &self.which {
            let name = OsStr::from_bytes(name);
            let found = which(name, here.path);
            let found = found.as_deref().map(|p| here.roots.split(p));
            let recorded = recorded
                .as_ref()
                .map(|(root, path)| (*root, path.as_path()));
            if found != recorded {
                return Err(format!(
                    "PATH now finds a different {}",
                    name.to_string_lossy()
                ));
            }
        }
        // What is in the workspace changes most often, and what is outside
        // both roots least. Looking in that order finds a change soonest.
        let by_root = |root: u8| self.inputs.iter().filter(move |input| input.root == root);
        let ordered = by_root(crate::roots::WORK)
            .chain(by_root(crate::roots::TARGET))
            .chain(by_root(crate::roots::ABSOLUTE));
        for input in ordered {
            let Some(path) = here.roots.join(input.root, &input.path) else {
                return Err("it was recorded with a target directory this command lacks".into());
            };
            let seen = here
                .examine(&path)
                .map_err(|e| format!("{} cannot be examined: {e}", path.display()))?;
            if seen.link != input.link {
                return Err(format!("{} is a different link", path.display()));
            }
            let changed = || Err(format!("{} changed", path.display()));
            match (&input.state, seen.kind) {
                (State::Absent, Kind::Absent) => (),
                (State::Absent, _) => return Err(format!("{} now exists", path.display())),
                (_, Kind::Absent) => return Err(format!("{} is gone", path.display())),
                (State::Dir(None), Kind::Dir) => (),
                (State::Dir(Some(recorded)), Kind::Dir) => {
                    let outputs = self
                        .outputs
                        .iter()
                        .map(|out| (out.root, out.path.as_path()));
                    let own = own_names((input.root, &input.path), &path, outputs, here.roots);
                    match list(&path, &own) {
                        Ok(names) if names == *recorded => (),
                        Ok(_) => {
                            return Err(format!("the entries of {} changed", path.display()));
                        }
                        Err(e) => return Err(format!("{}: {e}", path.display())),
                    }
                }
                (State::File(recorded), Kind::File) => {
                    let digest = match seen.digest {
                        Some(digest) => digest,
                        None => {
                            let digest = store
                                .digest_known(&path, seen.stamp.unwrap())
                                .map_err(|e| format!("{}: {e}", path.display()))?;
                            if let Some(seen) = here.seen.get_mut(&path) {
                                seen.digest = Some(digest);
                            }
                            digest
                        }
                    };
                    if digest != *recorded {
                        return changed();
                    }
                }
                _ => return changed(),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_survives_storage_and_its_key_ignores_what_it_wrote() {
        let mut m = Manifest {
            inputs: vec![
                Input {
                    root: 1,
                    path: "src/lib.rs".into(),
                    link: None,
                    state: State::File([7; 32]),
                },
                Input {
                    root: 0,
                    path: "/usr/lib/x".into(),
                    link: Some(b"y".to_vec()),
                    state: State::Dir(Some([9; 32])),
                },
                Input {
                    root: 2,
                    path: "debug/missing".into(),
                    link: None,
                    state: State::Absent,
                },
            ],
            env: vec![(b"B".to_vec(), None), (b"A".to_vec(), Some(b"1".to_vec()))],
            which: vec![(b"cc".to_vec(), Some((0, "/usr/bin/cc".into())))],
            pins: vec![(1, "/home/a/ws".into())],
            width: Some(Some(120)),
            ansi: Some(true),
            outputs: vec![Output {
                root: 2,
                path: "debug/a.rlib".into(),
                digest: [3; 32],
                size: 1 << 33,
                mode: 0o755,
                marked: true,
            }],
            removed: vec![(2, "debug/old".into())],
            detail: [5; 32],
            saved: vec![Output {
                root: 2,
                path: "debug/incremental/a-1x/s-1-2-3/dep-graph.bin".into(),
                digest: [4; 32],
                size: 77,
                mode: 0o644,
                marked: false,
            }],
            saved_in: [6; 32],
            saved_for: [8; 32],
        };
        m.normalize();
        let decoded = Manifest::decode(&m.encode().unwrap()).unwrap();
        assert_eq!(decoded, m);
        let key = m.key(&[1; 32]);
        assert_ne!(key, m.key(&[2; 32]));
        let mut other = m.clone();
        other.outputs.clear();
        other.saved.clear();
        assert_eq!(other.key(&[1; 32]), key);
        other.inputs[0].state = State::Absent;
        assert_ne!(other.key(&[1; 32]), key);
        assert!(Manifest::decode(b"junk").is_err());
    }
}
