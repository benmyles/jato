//! What a command's result depended on, and whether that still holds.
//!
//! A manifest lists every path the command examined with what was there:
//! a file's digest, a directory, the names in a directory, or nothing. It
//! also lists the environment variables, program lookups and directories the
//! result is tied to. A result is reused only when all of it holds again.
use crate::cas::Store;
use crate::observe::{Kind, Stamp, kind_of, listing_digest};
use crate::roots::{Roots, Surroundings, WORK};
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

/// The variable that lists the directories the loader looks through for
/// a program's libraries before any others.
pub const LIBRARY_PATH: &[u8] = b"LD_LIBRARY_PATH";

/// A name the command looked for in the directories above its workspace,
/// one after another: the way Cargo looks for its settings and for a
/// repository, and rustup for the toolchain to use. From another workspace
/// the same search passes through other directories. What it finds has to
/// be the same files.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Walk {
    /// The name, as a path from each of those directories.
    pub name: PathBuf,
    /// The last directory it was looked for in.
    pub top: PathBuf,
    /// Where something of that name was, nearest first, leaving out what
    /// the environment names in full and is reached by that name.
    pub found: Vec<PathBuf>,
}

/// One compiler command of a whole build, as the build's record keeps it:
/// which of the build's inputs it examined and which of its outputs it
/// wrote, by their positions in those lists. Only commands Cargo started
/// itself are kept so. Cargo knows what each of them writes, and runs one
/// again when that is gone.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Part {
    pub inputs: Vec<u32>,
    pub outputs: Vec<u32>,
}

/// An input that is not as it was recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Difference {
    /// Its position among the inputs.
    pub input: u32,
    /// It is still a file, with other contents.
    pub contents: bool,
    /// It is still a directory, with other entries.
    pub entries: bool,
}

/// How an input differs from what was recorded, in the two ways that
/// leave it the kind of thing it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Changed {
    Contents,
    Entries,
    Otherwise,
}

/// A program the build ran from its target directory, a build script,
/// with what it and everything it started examined of the build's own
/// inputs and wrote of its outputs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Script {
    /// The program's file, by its position among the outputs.
    pub program: u32,
    pub inputs: Vec<u32>,
    pub outputs: Vec<u32>,
}

/// What a build that starts from another has to do about the inputs that
/// differ.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stale {
    /// Which of the earlier build's parts have to be made again.
    pub again: Vec<bool>,
    /// Which of its build scripts have to run again: those that examined
    /// something that differs. Cargo runs a build script whose program
    /// has been made again, so the compiler that made it is among the
    /// parts above.
    pub scripts: Vec<bool>,
    /// The inputs among those that differ which are Cargo's own to make
    /// something of: manifests, lock files and the directories it finds
    /// packages and targets in. Cargo reads them again whenever it runs
    /// and decides again what to build.
    pub replanned: Vec<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    pub inputs: Vec<Input>,
    pub walks: Vec<Walk>,
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
    /// When the state was saved, in seconds since 1970. Newer state takes
    /// the place of older.
    pub saved_at: u64,
    /// The record of a whole build also says who examined what, so that a
    /// build of sources that differ a little can start from this one.
    /// These are the positions of the inputs whose change leaves nothing
    /// to start from: what Cargo read or listed, and what any program
    /// examined that Cargo did not start itself as a compiler.
    pub own: Vec<u32>,
    /// The positions of files Cargo itself asked about without opening
    /// them. It takes from that whether they are there and when they last
    /// changed, and looks again every time it runs.
    pub asked: Vec<u32>,
    /// The compiler commands Cargo started. An input in none of the three
    /// lists counts with the first.
    pub parts: Vec<Part>,
    /// The positions of the parts that another command's build made, on
    /// which this one rests: a check's, in the record of a build made
    /// where the check had been. This command does not make all of them
    /// again when their inputs change.
    pub theirs: Vec<u32>,
    /// The positions of the parts whose output names where it was made,
    /// when every such name in the build's output is theirs. A build
    /// somewhere else starts from this one with those to make again.
    pub pinned: Vec<u32>,
    /// The positions, among `own`, of the inputs that only the command's
    /// own program examined: Cargo, and nothing it started.
    pub cargos: Vec<u32>,
    /// How many times, one build starting from another, this build's
    /// line has been made with manifests that had changed. What Cargo
    /// keeps of a compilation that is no longer part of the build stays
    /// behind each time.
    pub replans: u32,
    /// The build scripts the build ran.
    pub scripts: Vec<Script>,
    /// The positions, among `own`, of the inputs the command's own
    /// program read or listed, whoever else examined them.
    pub read: Vec<u32>,
    /// The positions, among `own`, of the inputs examined by a program
    /// that was neither the command's own nor part of what a build script
    /// did, or by a build script that cannot be had to run again.
    pub strays: Vec<u32>,
    /// The positions of the outputs the command's own program wrote:
    /// what Cargo keeps for itself in a target directory.
    pub ours: Vec<u32>,
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
        self.walks.sort();
        self.walks.dedup();
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
        w.number(self.walks.len() as u64);
        for walk in &self.walks {
            w.bytes(walk.name.as_os_str().as_bytes());
            w.bytes(walk.top.as_os_str().as_bytes());
            w.number(walk.found.len() as u64);
            for found in &walk.found {
                w.bytes(found.as_os_str().as_bytes());
            }
        }
        w.0
    }

    /// Identifies a command together with everything its result depends on.
    pub fn key(&self, recipe: &Digest) -> Digest {
        let mut hash = blake3::Hasher::new_derive_key("jato command result v1");
        hash.update(recipe);
        hash.update(&self.conditions());
        // Saved state depends on nothing. One lot is told from the next by
        // when it was saved.
        if !self.saved.is_empty() {
            hash.update(&self.saved_at.to_le_bytes());
        }
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
        if !self.saved.is_empty() {
            w.number(self.saved_at);
        }
        positions(&mut w, &self.own);
        positions(&mut w, &self.asked);
        w.number(self.parts.len() as u64);
        for part in &self.parts {
            positions(&mut w, &part.inputs);
            positions(&mut w, &part.outputs);
        }
        positions(&mut w, &self.theirs);
        positions(&mut w, &self.pinned);
        positions(&mut w, &self.cargos);
        w.number(self.replans.into());
        w.number(self.scripts.len() as u64);
        for script in &self.scripts {
            w.number(script.program.into());
            positions(&mut w, &script.inputs);
            positions(&mut w, &script.outputs);
        }
        positions(&mut w, &self.read);
        positions(&mut w, &self.strays);
        positions(&mut w, &self.ours);
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
            let path = |bytes: &[u8]| PathBuf::from(OsStr::from_bytes(bytes));
            let mut walk = Walk {
                name: path(r.bytes()?),
                top: path(r.bytes()?),
                found: Vec::new(),
            };
            for _ in 0..r.count()? {
                walk.found.push(path(r.bytes()?));
            }
            m.walks.push(walk);
        }
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
        if !m.saved.is_empty() {
            m.saved_at = r.number()?;
        }
        m.own = read_positions(&mut r, m.inputs.len())?;
        m.asked = read_positions(&mut r, m.inputs.len())?;
        for _ in 0..r.count()? {
            m.parts.push(Part {
                inputs: read_positions(&mut r, m.inputs.len())?,
                outputs: read_positions(&mut r, m.outputs.len())?,
            });
        }
        m.theirs = read_positions(&mut r, m.parts.len())?;
        m.pinned = read_positions(&mut r, m.parts.len())?;
        m.cargos = read_positions(&mut r, m.inputs.len())?;
        m.replans = r.number()? as u32;
        for _ in 0..r.count()? {
            let program = r.number()? as u32;
            ensure!(
                (program as usize) < m.outputs.len(),
                "a script without a program"
            );
            m.scripts.push(Script {
                program,
                inputs: read_positions(&mut r, m.inputs.len())?,
                outputs: read_positions(&mut r, m.outputs.len())?,
            });
        }
        m.read = read_positions(&mut r, m.inputs.len())?;
        m.strays = read_positions(&mut r, m.inputs.len())?;
        m.ours = read_positions(&mut r, m.outputs.len())?;
        ensure!(r.0.is_empty(), "trailing manifest bytes");
        Ok(m)
    }
}

/// Write positions that rise, each as its distance from the one before.
fn positions(w: &mut Writer, list: &[u32]) {
    w.number(list.len() as u64);
    let mut last = 0;
    for &position in list {
        w.number(u64::from(position - last));
        last = position;
    }
}

/// Read positions written by `positions`, all of them below `end`.
fn read_positions(r: &mut Reader, end: usize) -> Result<Vec<u32>> {
    let count = r.count()?;
    let mut list = Vec::with_capacity(count.min(end));
    let mut last = 0u64;
    for _ in 0..count {
        last += r.number()?;
        ensure!(last < end as u64, "a position past the end of its list");
        list.push(last as u32);
    }
    Ok(list)
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

/// The directories a command with this environment finds programs in.
/// The one in which compilers' names lead back to Jato is left out: what
/// is found there is whatever the rest of PATH finds.
pub fn path_dirs(cwd: &Path, env: &Env) -> Vec<PathBuf> {
    let stand_ins = env.get(OsStr::new(crate::tools::DIRECTORY));
    env.get(OsStr::new("PATH"))
        .map(|path| {
            std::env::split_paths(path)
                .filter(|dir| Some(dir.as_os_str()) != stand_ins.map(|own| own.as_os_str()))
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
        // Not there, or in a place this user may not look, which is the
        // same to a command run by this user and is recorded the same.
        let absent = |e: &io::Error| {
            matches!(
                e.kind(),
                io::ErrorKind::NotFound
                    | io::ErrorKind::NotADirectory
                    | io::ErrorKind::PermissionDenied
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
    /// Whether a search upward from `work` finds what each recorded one
    /// found. `named` says what has a name of its own in this environment.
    fn walks_hold(&self, work: &Path, named: impl Fn(&Path) -> bool) -> Result<(), String> {
        for walk in &self.walks {
            let mut found = Vec::new();
            let mut reached = false;
            for dir in work.ancestors().skip(1) {
                let here = dir.join(&walk.name);
                if !named(&here) && fs::symlink_metadata(&here).is_ok() {
                    found.push(here);
                }
                if dir == walk.top {
                    reached = true;
                    break;
                }
            }
            if !reached {
                return Err(format!(
                    "this workspace is not below {}, as far up as {} was looked for",
                    walk.top.display(),
                    walk.name.display()
                ));
            }
            if let Some(extra) = found.iter().find(|here| !walk.found.contains(here)) {
                return Err(format!("{} is above this workspace", extra.display()));
            }
            if let Some(missing) = walk.found.iter().find(|there| !found.contains(there)) {
                return Err(format!("{} is not above this workspace", missing.display()));
            }
        }
        Ok(())
    }

    /// Whether everything this manifest records holds here. An error says
    /// what does not.
    pub fn holds(&self, here: &mut Here<'_>, store: &mut Store) -> Result<(), String> {
        self.settled(here)?;
        // What is in the workspace changes most often, and what is outside
        // both roots least. Looking in that order finds a change soonest.
        let by_root = |root: u8| self.inputs.iter().filter(move |input| input.root == root);
        let ordered = by_root(crate::roots::WORK)
            .chain(by_root(crate::roots::TARGET))
            .chain(by_root(crate::roots::ABSOLUTE));
        for input in ordered {
            self.as_recorded(input, here, store)
                .map_err(|(why, _)| why)?;
        }
        Ok(())
    }

    /// Every input that is not as it was recorded, where everything else
    /// the manifest records holds, but for where it was made. An error
    /// says what else does not, or that more than `most` inputs differ.
    pub fn differences(
        &self,
        here: &mut Here<'_>,
        store: &mut Store,
        most: usize,
    ) -> Result<Vec<Difference>, String> {
        self.settled_anywhere(here)?;
        let mut found = Vec::new();
        for (at, input) in self.inputs.iter().enumerate() {
            if let Err((why, changed)) = self.as_recorded(input, here, store) {
                if found.len() == most {
                    return Err(format!("{why}, and more than {most} other files"));
                }
                found.push(Difference {
                    input: at as u32,
                    contents: changed == Changed::Contents,
                    entries: changed == Changed::Entries,
                });
            }
        }
        Ok(found)
    }

    /// Which parts a build that starts from this one has to make again,
    /// given the inputs that differ, and which of those inputs are left
    /// to Cargo. An error is the position of an input whose change leaves
    /// nothing to start from, and why.
    pub fn stale_parts(&self, differences: &[Difference]) -> Result<Stale, (u32, &'static str)> {
        let mut stale = Stale {
            again: vec![false; self.parts.len()],
            scripts: vec![false; self.scripts.len()],
            replanned: Vec::new(),
        };
        for difference in differences {
            let at = difference.input;
            let mut examined = false;
            for (part, again) in self.parts.iter().zip(&mut stale.again) {
                if part.inputs.binary_search(&at).is_ok() {
                    *again = true;
                    examined = true;
                }
            }
            if self.own.binary_search(&at).is_ok() {
                // A program nobody can have run again examined it.
                if self.strays.binary_search(&at).is_ok() {
                    return Err((
                        at,
                        "which a program examined that cannot be had to run again",
                    ));
                }
                // The build scripts that examined it run again, and see
                // for themselves.
                let mut scripted = false;
                for (script, again) in self.scripts.iter().zip(&mut stale.scripts) {
                    if script.inputs.binary_search(&at).is_ok() {
                        *again = true;
                        scripted = true;
                    }
                }
                // What Cargo itself made of it, it has to be able to make
                // again: all there is to it where no script looked.
                let cargo_s = self.read.binary_search(&at).is_ok() || !scripted;
                if cargo_s {
                    if !self.cargo_s_to_plan_with(difference, scripted) {
                        return Err((at, "which the build itself examined"));
                    }
                    stale.replanned.push(at);
                }
                continue;
            }
            // Cargo goes by whether a file it asks about is there. What
            // the file holds is the business of whoever reads it.
            let asked = self.asked.binary_search(&at).is_ok();
            // What it finds there this time is for Cargo to plan with.
            if asked && !difference.contents {
                if !self.cargo_s_to_plan_with(difference, false) {
                    return Err((at, "which Cargo had found there"));
                }
                stale.replanned.push(at);
            }
            if !(asked || examined) {
                return Err((at, "which the build itself examined"));
            }
        }
        // A build script runs again when Cargo finds its program made
        // again: by the compiler that wrote it, or wrote a file of the
        // same bytes that Cargo then copied to where it runs it.
        for (script, again) in self.scripts.iter().zip(&stale.scripts) {
            if !*again {
                continue;
            }
            let program = &self.outputs[script.program as usize];
            let makes = |part: &&Part| {
                part.outputs
                    .iter()
                    .any(|at| self.outputs[*at as usize].digest == program.digest)
            };
            match self.parts.iter().position(|part| makes(&part)) {
                Some(part) if program.size > 0 => stale.again[part] = true,
                _ => {
                    let at = differences
                        .iter()
                        .map(|difference| difference.input)
                        .find(|at| script.inputs.binary_search(at).is_ok());
                    return Err((
                        at.unwrap_or(0),
                        "and no compiler of the build made the build script that read it",
                    ));
                }
            }
        }
        Ok(stale)
    }

    /// Whether an input that differs, and that Cargo's own program
    /// examined, is Cargo's to make something of again. Cargo reads its
    /// manifests and lock files, lists the directories it finds packages
    /// and targets in and asks which files are there every time it runs,
    /// and what follows from them for a compilation is in what Cargo
    /// remembers of it: it runs the compilations for which that has
    /// changed, and those that are new.
    ///
    /// So anything that only Cargo examined may differ, with two
    /// exceptions. Its settings reach compilers in ways it does not
    /// remember. And what it found in the target directory was the state
    /// it built on. Where build scripts examined the input as well, and
    /// run again for it, `scripted` says so: then what Cargo read has to
    /// be a manifest, a lock file or a directory.
    fn cargo_s_to_plan_with(&self, difference: &Difference, scripted: bool) -> bool {
        let input = &self.inputs[difference.input as usize];
        let settings = input
            .path
            .components()
            .any(|part| part.as_os_str() == ".cargo")
            || input
                .path
                .file_name()
                .is_some_and(|name| name == "rust-toolchain" || name == "rust-toolchain.toml");
        if settings || input.root == crate::roots::TARGET {
            return false;
        }
        if !scripted {
            let only = |list: &[u32]| list.binary_search(&difference.input).is_ok();
            return only(&self.cargos) || only(&self.asked);
        }
        let name = input.path.file_name();
        name.is_some_and(|name| name == "Cargo.toml" || name == "Cargo.lock")
            || difference.entries
            || matches!(input.state, State::Dir(_))
    }

    /// What `position` among the inputs is called here.
    pub fn named(&self, position: u32, roots: &Roots) -> String {
        let input = &self.inputs[position as usize];
        roots
            .join(input.root, &input.path)
            .unwrap_or_else(|| input.path.clone())
            .display()
            .to_string()
    }

    /// Whether what the result is tied to apart from its inputs is as it
    /// was: where it is, the environment, the terminal, and what PATH and
    /// a search above the workspace find.
    fn settled(&self, here: &mut Here<'_>) -> Result<(), String> {
        self.placed(here.roots)?;
        self.settled_anywhere(here)
    }

    /// Whether this is where the result's output says it was made.
    pub fn placed(&self, roots: &Roots) -> Result<(), String> {
        for (root, recorded) in &self.pins {
            if roots.path(*root) != Some(recorded.as_path()) {
                return Err(format!(
                    "its output names {}, which is not where this build is",
                    recorded.display()
                ));
            }
        }
        Ok(())
    }

    /// The same as `settled`, wherever the result was made.
    fn settled_anywhere(&self, here: &mut Here<'_>) -> Result<(), String> {
        if let Some(work) = here.roots.path(WORK)
            && !self.walks.is_empty()
        {
            let around = Surroundings::of(work, here.env);
            self.walks_hold(work, |path| around.named(path))?;
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
            let variable = OsStr::from_bytes(name);
            let current = here.env.get(variable).map(|v| here.roots.mark_os(v));
            if current.as_deref() == recorded.as_deref() {
                continue;
            }
            if name == LIBRARY_PATH {
                self.libraries_found_alike(recorded.as_deref(), here)?;
                continue;
            }
            return Err(format!(
                "the environment variable {} changed",
                variable.to_string_lossy()
            ));
        }
        for (name, recorded) in &self.which {
            let name = OsStr::from_bytes(name);
            let found = which(name, here.path);
            let found = found.as_deref().map(|p| here.roots.split(p));
            let recorded = recorded
                .as_ref()
                .map(|(root, path)| (*root, path.as_path()));
            if found != recorded {
                let at = |place: Option<(u8, &Path)>| match place {
                    Some((root, path)) => here
                        .roots
                        .join(root, path)
                        .unwrap_or_else(|| path.to_owned())
                        .display()
                        .to_string(),
                    None => "nowhere".to_owned(),
                };
                return Err(format!(
                    "PATH now finds a different {}: at {}, where it was at {}",
                    name.to_string_lossy(),
                    at(found),
                    at(recorded)
                ));
            }
        }
        Ok(())
    }

    /// Whether the loader, going through the directories its variable
    /// names now, finds every library where it found it when the command
    /// was recorded with another list.
    ///
    /// Cargo puts a directory more in the list for what a build script
    /// starts in a build than in a check, and the compilers a build
    /// script starts are the same commands in both. The loader takes the
    /// first directory that has the name it wants. So the directories in
    /// both lists have to follow one another as they did, nothing may
    /// have been found in a directory no longer listed, and a directory
    /// listed only now may hold none of the names: those of the shared
    /// objects among the inputs, wherever each was found, and of anything
    /// examined in a directory of the recorded list.
    fn libraries_found_alike(
        &self,
        recorded: Option<&[u8]>,
        here: &Here<'_>,
    ) -> Result<(), String> {
        let changed = |how: String| format!("the libraries' search path changed: {how}");
        // An entry that is no full path is a place that moves with
        // whatever looks there.
        let dirs = |list: &[u8]| -> Option<Vec<PathBuf>> {
            list.split(|byte| *byte == b':')
                .map(|dir| {
                    dir.starts_with(b"/")
                        .then(|| PathBuf::from(OsStr::from_bytes(dir)))
                })
                .collect()
        };
        let was = match recorded {
            None => Some(Vec::new()),
            Some(list) => here.roots.unmark(list).and_then(|list| dirs(&list)),
        };
        let now = match here.env.get(OsStr::from_bytes(LIBRARY_PATH)) {
            None => Some(Vec::new()),
            Some(list) => dirs(list.as_bytes()),
        };
        let (Some(was), Some(now)) = (was, now) else {
            return Err(changed("it names a place that is not a full path".into()));
        };
        let kept = |from: &[PathBuf], other: &[PathBuf]| -> Vec<PathBuf> {
            from.iter()
                .filter(|dir| other.contains(dir))
                .cloned()
                .collect()
        };
        if kept(&was, &now) != kept(&now, &was) {
            return Err(changed("its directories are in another order".into()));
        }
        let added: Vec<&PathBuf> = now.iter().filter(|dir| !was.contains(dir)).collect();
        let mut names: Vec<&OsStr> = Vec::new();
        for input in &self.inputs {
            let (Some(parent), Some(name)) = (input.path.parent(), input.path.file_name()) else {
                continue;
            };
            let Some(parent) = here.roots.join(input.root, parent) else {
                continue;
            };
            let listed = was.contains(&parent);
            if listed && input.state != State::Absent && !now.contains(&parent) {
                return Err(changed(format!(
                    "{} was found in a directory that is no longer in it",
                    parent.join(name).display()
                )));
            }
            if listed || memchr::memmem::find(name.as_bytes(), b".so").is_some() {
                names.push(name);
            }
        }
        for dir in added {
            // The loader also looks in directories of this name for
            // libraries made for the processor at hand.
            let found = std::iter::once(OsStr::new("glibc-hwcaps"))
                .chain(names.iter().copied())
                .find(|name| fs::symlink_metadata(dir.join(name)).is_ok());
            if let Some(name) = found {
                return Err(changed(format!(
                    "{} would be found first now",
                    dir.join(name).display()
                )));
            }
        }
        Ok(())
    }

    /// Whether one input is as it was recorded. An error says how it
    /// differs, and whether it is only that a file holds something else.
    fn as_recorded(
        &self,
        input: &Input,
        here: &mut Here<'_>,
        store: &mut Store,
    ) -> Result<(), (String, Changed)> {
        let other = |why: String| Err((why, Changed::Otherwise));
        let Some(path) = here.roots.join(input.root, &input.path) else {
            return other("it was recorded with a target directory this command lacks".into());
        };
        let seen = match here.examine(&path) {
            Ok(seen) => seen,
            Err(e) => return other(format!("{} cannot be examined: {e}", path.display())),
        };
        if seen.link != input.link {
            return other(format!("{} is a different link", path.display()));
        }
        let changed = || format!("{} changed", path.display());
        match (&input.state, seen.kind) {
            (State::Absent, Kind::Absent) => Ok(()),
            (State::Absent, _) => other(format!("{} now exists", path.display())),
            (_, Kind::Absent) => other(format!("{} is gone", path.display())),
            (State::Dir(None), Kind::Dir) => Ok(()),
            (State::Dir(Some(recorded)), Kind::Dir) => {
                let outputs = self
                    .outputs
                    .iter()
                    .map(|out| (out.root, out.path.as_path()));
                let own = own_names((input.root, &input.path), &path, outputs, here.roots);
                match list(&path, &own) {
                    Ok(names) if names == *recorded => Ok(()),
                    Ok(_) => Err((
                        format!("the entries of {} changed", path.display()),
                        Changed::Entries,
                    )),
                    Err(e) => other(format!("{}: {e}", path.display())),
                }
            }
            (State::File(recorded), Kind::File) => {
                let digest = match seen.digest {
                    Some(digest) => digest,
                    None => {
                        let stamp = seen.stamp.unwrap();
                        let digest = if input.root == crate::roots::TARGET {
                            store.digest_placed(&path, stamp, here.roots)
                        } else {
                            store.digest_seen(&path, stamp)
                        };
                        let digest = match digest {
                            Ok(digest) => digest,
                            Err(e) => return other(format!("{}: {e}", path.display())),
                        };
                        if let Some(seen) = here.seen.get_mut(&path) {
                            seen.digest = Some(digest);
                        }
                        digest
                    }
                };
                if digest == *recorded {
                    Ok(())
                } else {
                    Err((changed(), Changed::Contents))
                }
            }
            _ => other(changed()),
        }
    }

    /// What a file among the inputs holds now, if it is one.
    pub fn now(&self, position: u32, here: &mut Here<'_>) -> Option<State> {
        let input = &self.inputs[position as usize];
        let path = here.roots.join(input.root, &input.path)?;
        here.seen.get(&path)?.digest.map(State::File)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_above_the_workspace_has_to_find_the_same_from_another() {
        let temp = tempfile::tempdir().unwrap();
        let top = temp.path();
        let settings = Path::new(".cargo/config.toml");
        // As recorded in `first/ws`: Cargo looked for settings in every
        // directory up to `top`, and found those of `top` itself.
        let there = top.join(settings);
        fs::create_dir_all(there.parent().unwrap()).unwrap();
        fs::write(&there, "").unwrap();
        let m = Manifest {
            walks: vec![Walk {
                name: settings.to_owned(),
                top: top.to_owned(),
                found: vec![there.clone()],
            }],
            ..Manifest::default()
        };
        let nothing = |_: &Path| false;
        let work = |name: &str| {
            let dir = top.join(name);
            fs::create_dir_all(&dir).unwrap();
            dir
        };
        // The same from a workspace beside it, and from one further down.
        assert_eq!(m.walks_hold(&work("second/ws"), nothing), Ok(()));
        let deeper = work("third/further/down/ws");
        assert_eq!(m.walks_hold(&deeper, nothing), Ok(()));
        // Settings nearer to that workspace than those recorded.
        let nearer = top.join("third/further").join(settings);
        fs::create_dir_all(nearer.parent().unwrap()).unwrap();
        fs::write(&nearer, "").unwrap();
        let why = m.walks_hold(&deeper, nothing).unwrap_err();
        assert!(why.ends_with("is above this workspace"), "{why}");
        // Unless they have a name of their own, as Cargo's home has.
        assert_eq!(m.walks_hold(&deeper, |path| path == nearer), Ok(()));
        // A workspace that is not below the directory searched up to.
        let elsewhere = tempfile::tempdir().unwrap();
        let why = m.walks_hold(elsewhere.path(), nothing).unwrap_err();
        assert!(why.contains("is not below"), "{why}");
        // What was found is gone.
        fs::remove_file(&there).unwrap();
        let why = m.walks_hold(&work("second/ws"), nothing).unwrap_err();
        assert!(why.ends_with("is not above this workspace"), "{why}");
    }

    #[test]
    fn a_library_path_is_known_by_what_the_loader_finds_along_it() {
        use crate::roots::{ABSOLUTE, Root, TARGET};
        let temp = tempfile::tempdir().unwrap();
        let dir = |name: &str| {
            let dir = temp.path().join(name);
            fs::create_dir_all(&dir).unwrap();
            dir
        };
        let (tools, system, target) = (dir("toolchain/lib"), dir("usr/lib"), dir("ws/target"));
        fs::create_dir_all(target.join("debug")).unwrap();
        for file in [
            tools.join("libdriver.so"),
            tools.join("libstd.rlib"),
            system.join("libc.so.6"),
        ] {
            fs::write(file, "").unwrap();
        }
        let roots = Roots {
            work: Some(Root::at(temp.path().join("ws"))),
            target: Some(Root::at(target.clone())),
        };
        let file = |path: PathBuf, state: State| Input {
            root: ABSOLUTE,
            path,
            link: None,
            state,
        };
        // As recorded by a compiler that a build script started in a
        // check: its own libraries, one of the system's, and a name it
        // looked for among its own in vain.
        let found = State::File([0; 32]);
        let m = Manifest {
            inputs: vec![
                file(tools.join("libdriver.so"), found.clone()),
                file(tools.join("libstd.rlib"), found.clone()),
                file(tools.join("libextra.so"), State::Absent),
                file(system.join("libc.so.6"), found.clone()),
                file(temp.path().join("ws/src/lib.rs"), found),
            ],
            ..Manifest::default()
        };
        let recorded = tools.as_os_str().as_bytes().to_vec();
        let alike = |now: Option<String>| {
            let mut env = Env::new();
            if let Some(now) = now {
                env.insert(OsStr::from_bytes(LIBRARY_PATH).to_owned(), now.into());
            }
            let here = Here::new(&roots, &env, &[], None, false);
            m.libraries_found_alike(Some(&recorded), &here)
        };
        let (tools_s, debug) = (tools.display(), target.join("debug"));
        // The same list, and the one a build has, with a directory before
        // it that holds none of the names.
        assert_eq!(alike(Some(format!("{tools_s}"))), Ok(()));
        assert_eq!(
            alike(Some(format!("{}:{tools_s}", debug.display()))),
            Ok(())
        );
        assert_eq!(
            alike(Some(format!("{tools_s}:{}", debug.display()))),
            Ok(())
        );
        // A file of another name there changes nothing.
        fs::write(debug.join("app"), "").unwrap();
        fs::write(debug.join("lib.rs"), "").unwrap();
        assert_eq!(
            alike(Some(format!("{}:{tools_s}", debug.display()))),
            Ok(())
        );
        // One of a name that was looked up does: found among the
        // compiler's own, looked for there in vain, or found elsewhere.
        for name in ["libstd.rlib", "libextra.so", "libc.so.6", "glibc-hwcaps"] {
            fs::write(debug.join(name), "").unwrap();
            let why = alike(Some(format!("{}:{tools_s}", debug.display()))).unwrap_err();
            assert!(why.ends_with("would be found first now"), "{name}: {why}");
            fs::remove_file(debug.join(name)).unwrap();
        }
        // The directory things were found in is no longer looked through.
        let why = alike(None).unwrap_err();
        assert!(why.contains("no longer in it"), "{why}");
        let why = alike(Some(format!("{}", system.display()))).unwrap_err();
        assert!(why.contains("no longer in it"), "{why}");
        // A place that moves with whoever looks.
        let why = alike(Some(format!("{tools_s}:"))).unwrap_err();
        assert!(why.contains("not a full path"), "{why}");
        // Two directories the other way round.
        let both = format!("{tools_s}:{}", system.display());
        let m2 = Manifest {
            inputs: m.inputs.clone(),
            ..Manifest::default()
        };
        let here_env: Env = [(
            OsStr::from_bytes(LIBRARY_PATH).to_owned(),
            format!("{}:{tools_s}", system.display()).into(),
        )]
        .into_iter()
        .collect();
        let here = Here::new(&roots, &here_env, &[], None, false);
        let why = m2
            .libraries_found_alike(Some(both.as_bytes()), &here)
            .unwrap_err();
        assert!(why.contains("another order"), "{why}");
        // A list recorded with the target directory in it, read where
        // the target directory is another.
        let marked = roots
            .mark(format!("{}:{tools_s}", debug.display()).as_bytes())
            .into_owned();
        assert!(memchr::memchr(0, &marked).is_some());
        let elsewhere = Roots {
            work: roots.work.clone(),
            target: Some(Root::at(dir("other/target"))),
        };
        let env: Env = [(
            OsStr::from_bytes(LIBRARY_PATH).to_owned(),
            format!(
                "{}:{tools_s}",
                elsewhere.path(TARGET).unwrap().join("debug").display()
            )
            .into(),
        )]
        .into_iter()
        .collect();
        let here = Here::new(&elsewhere, &env, &[], None, false);
        assert_eq!(m.libraries_found_alike(Some(&marked), &here), Ok(()));
    }

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
            walks: vec![Walk {
                name: ".cargo/config.toml".into(),
                top: "/".into(),
                found: vec!["/home/.cargo/config.toml".into()],
            }],
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
            saved_at: 1_795_000_000,
            own: vec![0, 2],
            asked: vec![1],
            parts: vec![
                Part {
                    inputs: vec![1],
                    outputs: vec![0],
                },
                Part::default(),
            ],
            theirs: vec![1],
            pinned: vec![0],
            cargos: vec![2],
            replans: 3,
            scripts: vec![Script {
                program: 0,
                inputs: vec![0, 2],
                outputs: vec![0],
            }],
            read: vec![0],
            strays: vec![2],
            ours: vec![0],
        };
        m.normalize();
        let decoded = Manifest::decode(&m.encode().unwrap()).unwrap();
        assert_eq!(decoded, m);
        let key = m.key(&[1; 32]);
        assert_ne!(key, m.key(&[2; 32]));
        let mut other = m.clone();
        other.outputs.clear();
        other.parts.clear();
        other.own.clear();
        assert_eq!(other.key(&[1; 32]), key);
        // A position that leads nowhere is not believed.
        let mut wrong = m.clone();
        wrong.parts[0].outputs = vec![1];
        assert!(Manifest::decode(&wrong.encode().unwrap()).is_err());
        other.saved_at += 1;
        assert_ne!(other.key(&[1; 32]), key);
        other.saved.clear();
        other.inputs[0].state = State::Absent;
        assert_ne!(other.key(&[1; 32]), key);
        assert!(Manifest::decode(b"junk").is_err());
    }

    #[test]
    fn what_differs_says_which_compilers_run_again_or_that_nothing_is_left() {
        let input = |path: &str| Input {
            root: 1,
            path: path.into(),
            link: None,
            state: State::File([1; 32]),
        };
        // Five inputs: the first is the build's own, the second is one
        // Cargo asked about and the first compiler read, the third only
        // the second compiler read, the fourth both read, and the last
        // Cargo asked about and nobody opened.
        let m = Manifest {
            inputs: ["Cargo.toml", "a/lib.rs", "b/lib.rs", "shared.rs", "README"]
                .map(input)
                .to_vec(),
            own: vec![0],
            asked: vec![1, 4],
            parts: vec![
                Part {
                    inputs: vec![1, 3],
                    outputs: vec![0],
                },
                Part {
                    inputs: vec![2, 3],
                    outputs: vec![1],
                },
            ],
            ..Manifest::default()
        };
        let other = |input: u32| Difference {
            input,
            contents: true,
            entries: false,
        };
        let gone = |input: u32| Difference {
            input,
            contents: false,
            entries: false,
        };
        let again = |differences: &[Difference]| {
            m.stale_parts(differences)
                .map(|s| s.again)
                .map_err(|(at, _)| at)
        };
        assert_eq!(again(&[]), Ok(vec![false, false]));
        assert_eq!(again(&[other(1)]), Ok(vec![true, false]));
        assert_eq!(again(&[other(2)]), Ok(vec![false, true]));
        assert_eq!(again(&[gone(2)]), Ok(vec![false, true]));
        assert_eq!(again(&[other(3)]), Ok(vec![true, true]));
        // A file only Cargo asked about holds something else: nobody
        // read it.
        assert_eq!(again(&[other(4)]), Ok(vec![false, false]));
        // What the build itself read, and a file Cargo asked about that
        // is gone.
        assert_eq!(again(&[other(2), other(0)]), Err(0));
        // A file Cargo asked about that is gone is Cargo's to miss: the
        // compiler that read it is left out, and Cargo plans again.
        assert_eq!(
            m.stale_parts(&[gone(1)]),
            Ok(Stale {
                again: vec![true, false],
                scripts: vec![],
                replanned: vec![1],
            })
        );
        assert_eq!(m.stale_parts(&[gone(4)]).map(|s| s.replanned), Ok(vec![4]));
        // An input in no list is the build's own.
        let mut loose = m.clone();
        loose.own.clear();
        assert_eq!(loose.stale_parts(&[other(0)]).map_err(|(at, _)| at), Err(0));

        // A manifest that only Cargo examined is Cargo's to plan with
        // again: nothing is made again for it here, and Cargo is left to
        // see what follows. One that is gone, or that something else
        // read as well, leaves nothing to start from.
        let mut planned = m.clone();
        planned.inputs.push(input("crates"));
        planned.inputs.push(input(".cargo/config.toml"));
        planned.own = vec![0, 5, 6];
        planned.cargos = vec![0, 5, 6];
        let entries = |input: u32| Difference {
            input,
            contents: false,
            entries: true,
        };
        assert_eq!(
            planned.stale_parts(&[other(0), other(2)]),
            Ok(Stale {
                again: vec![false, true],
                scripts: vec![],
                replanned: vec![0],
            })
        );
        assert_eq!(
            planned.stale_parts(&[entries(5)]).map(|s| s.replanned),
            Ok(vec![5])
        );
        // One that is gone is Cargo's to miss.
        assert_eq!(
            planned.stale_parts(&[gone(0)]).map(|s| s.replanned),
            Ok(vec![0])
        );
        // Settings are Cargo's own too, and no plan: they reach
        // compilers in ways Cargo does not remember.
        assert_eq!(
            planned.stale_parts(&[other(6)]).map_err(|(at, _)| at),
            Err(6)
        );
        planned.cargos = vec![5];
        assert_eq!(
            planned.stale_parts(&[other(0)]).map_err(|(at, _)| at),
            Err(0)
        );

        // A build script read the manifest too, and a file of its own.
        // It runs again for either, by having the compiler that made its
        // program run again: the first here, whose output is the program.
        let mut scripted = planned.clone();
        scripted.inputs.push(input("data.txt"));
        scripted.own = vec![0, 5, 6, 7];
        scripted.read = vec![0, 5, 6];
        scripted.outputs = vec![
            Output {
                root: 2,
                path: "debug/build/app/script".into(),
                digest: [7; 32],
                size: 9,
                mode: 0o755,
                marked: false,
            },
            Output {
                root: 2,
                path: "debug/deps/libb.rlib".into(),
                digest: [8; 32],
                size: 9,
                mode: 0o644,
                marked: false,
            },
        ];
        scripted.scripts = vec![Script {
            program: 0,
            inputs: vec![0, 7],
            outputs: vec![],
        }];
        assert_eq!(
            scripted.stale_parts(&[other(7)]),
            Ok(Stale {
                again: vec![true, false],
                scripts: vec![true],
                replanned: vec![],
            })
        );
        assert_eq!(
            scripted.stale_parts(&[other(0)]),
            Ok(Stale {
                again: vec![true, false],
                scripts: vec![true],
                replanned: vec![0],
            })
        );
        // A file the script read that is gone is the script's to miss.
        assert_eq!(
            scripted.stale_parts(&[gone(7)]).map(|s| s.scripts),
            Ok(vec![true])
        );
        // Something that cannot be run again read it as well.
        scripted.strays = vec![7];
        assert_eq!(
            scripted.stale_parts(&[other(7)]).map_err(|(at, _)| at),
            Err(7)
        );
        // No compiler of the build made the program.
        scripted.strays.clear();
        scripted.outputs[0].digest = [9; 32];
        scripted.parts[0].outputs = vec![1];
        scripted.parts[1].outputs = vec![];
        assert_eq!(
            scripted.stale_parts(&[other(7)]).map_err(|(at, _)| at),
            Err(7)
        );
    }
}
