//! The two directories whose location must not matter: the directory Cargo
//! runs the compiler in, and the target directory.
//!
//! Paths beneath them are stored relative to them, so a result recorded in
//! one checkout or target directory is found from another. Whether a result
//! can really be used elsewhere is decided separately, from its bytes.
use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub const ABSOLUTE: u8 = 0;
pub const WORK: u8 = 1;
pub const TARGET: u8 = 2;

/// Stands for a root in stored text. Text never contains a NUL.
const MARKS: [&[u8]; 3] = [b"", b"\0jato:work\0", b"\0jato:target\0"];

#[derive(Clone, Debug, Default)]
pub struct Root {
    /// As Cargo spells it.
    pub path: PathBuf,
    /// With symbolic links resolved, when that differs.
    real: Option<PathBuf>,
}

#[derive(Clone, Debug, Default)]
pub struct Roots {
    pub work: Option<Root>,
    pub target: Option<Root>,
}

/// Paths are found in text by their bytes. One that needs escaping in JSON
/// or in a Makefile would not be found, so it is not treated as a root.
fn plain(path: &Path) -> bool {
    let bytes = path.as_os_str().as_bytes();
    bytes.len() > 1
        && bytes[0] == b'/'
        && bytes[bytes.len() - 1] != b'/'
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+@,=~".contains(b))
        && !path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}

impl Root {
    pub fn new(path: &Path) -> Option<Self> {
        if !plain(path) {
            return None;
        }
        let real = std::fs::canonicalize(path)
            .ok()
            .filter(|real| real != path && plain(real));
        Some(Self {
            path: path.to_owned(),
            real,
        })
    }

    fn strip<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        path.strip_prefix(&self.path)
            .ok()
            .or_else(|| path.strip_prefix(self.real.as_ref()?).ok())
    }
}

/// The directory Cargo marks as a target directory at or above `hint`.
fn target_above(hint: &Path) -> Option<&Path> {
    hint.ancestors()
        .take(8)
        .find(|dir| dir.join("CACHEDIR.TAG").is_file())
}

/// The nearest directory at or above `from` whose manifest declares a
/// workspace, or failing that the nearest with a manifest at all.
pub fn workspace_above(from: &Path) -> Option<PathBuf> {
    let mut nearest = None;
    for dir in from.ancestors() {
        let manifest = dir.join("Cargo.toml");
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        if text
            .lines()
            .any(|line| line.trim_start().starts_with("[workspace"))
        {
            return Some(dir.to_owned());
        }
        nearest.get_or_insert_with(|| dir.to_owned());
    }
    nearest
}

/// How many directories deep a path is.
fn depth(path: &Path) -> usize {
    path.components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .count()
}

/// What lies around a workspace: the directories a path written with `..`
/// from inside it would lead to.
///
/// A command reaches such a place by going up from the workspace: Cargo
/// for a package named by `path = "../shared"`, a compiler for a file
/// included from beside the crate, Cargo and rustup for the settings they
/// look for in every directory above. In another checkout the same steps
/// lead somewhere else. What a command examined there is therefore
/// recorded a second time, by the way from the workspace, and a result is
/// reused only where both places are as they were.
pub struct Surroundings {
    work: PathBuf,
    /// The directory, some way above the workspace, below which anything
    /// may have been reached from it.
    reach: Option<PathBuf>,
    /// Directories the environment names in full. What is in them was
    /// reached by that name, wherever the workspace is.
    named: Vec<PathBuf>,
    /// The home directory, if the workspace is somewhere in it.
    home: Option<PathBuf>,
}

impl Surroundings {
    pub fn of(work: &Path, env: &std::collections::BTreeMap<OsString, OsString>) -> Self {
        // What moves with the workspace: the repository it is part of.
        let moved = work
            .ancestors()
            .take(8)
            .find(|dir| std::fs::symlink_metadata(dir.join(".git")).is_ok())
            .unwrap_or(work);
        // Two directories further up, but not as far as the top two
        // levels of the filesystem, which hold everybody's files.
        let above: Vec<&Path> = moved.ancestors().skip(1).take(2).collect();
        let reach = above
            .iter()
            .rev()
            .find(|dir| depth(dir) >= 3)
            .or_else(|| above.first().filter(|dir| depth(dir) >= 2))
            .map(|dir| dir.to_path_buf())
            .or_else(|| (moved != work).then(|| moved.to_owned()));
        let mut named = Vec::new();
        let mut name = |dir: PathBuf| {
            if dir.is_absolute() && depth(&dir) > 0 && !work.starts_with(&dir) {
                named.push(dir);
            }
        };
        for (variable, value) in env {
            // Where a shell is and was says nothing about the build.
            if variable == "PWD" || variable == "OLDPWD" {
                continue;
            }
            std::env::split_paths(value).for_each(&mut name);
        }
        // Where Cargo and rustup keep their own files when nothing says.
        if let Some(home) = env.get(OsStr::new("HOME")) {
            for (variable, dir) in [("CARGO_HOME", ".cargo"), ("RUSTUP_HOME", ".rustup")] {
                if !env.contains_key(OsStr::new(variable)) {
                    name(Path::new(home).join(dir));
                }
            }
        }
        let home = env
            .get(OsStr::new("HOME"))
            .map(PathBuf::from)
            .filter(|home| home.is_absolute() && depth(home) > 0 && work.starts_with(home));
        Self {
            work: work.to_owned(),
            reach,
            named,
            home,
        }
    }

    /// Whether `path` was reached by a name of its own.
    pub fn named(&self, path: &Path) -> bool {
        self.named.iter().any(|dir| path.starts_with(dir))
    }

    /// Whether `path` is one of the places in the home directory where
    /// programs keep a person's settings and their own files: a name
    /// there that begins with a dot, as git's `.gitconfig` is and
    /// `.config` and `.cache`. A program finds such a place by the home
    /// directory's name, wherever the workspace is. One that looks for a
    /// name in every directory above the workspace comes by there too,
    /// and that is a search, recorded as one.
    pub fn kept(&self, path: &Path) -> bool {
        let Some(home) = &self.home else {
            return false;
        };
        let first = path
            .strip_prefix(home)
            .ok()
            .and_then(|rest| rest.components().next());
        match first {
            Some(std::path::Component::Normal(first)) => {
                first.as_encoded_bytes().starts_with(b".")
                    && !self.work.starts_with(home.join(first))
            }
            _ => false,
        }
    }

    /// Whether `path`, which is outside the workspace, is close enough
    /// that anything may have led there from it.
    pub fn beside(&self, path: &Path) -> bool {
        self.reach
            .as_ref()
            .is_some_and(|reach| path.starts_with(reach))
    }

    /// How far above the workspace the directory is that `path` and the
    /// workspace are both in, and the way down from there.
    pub fn above<'a>(&self, path: &'a Path) -> (usize, &'a Path) {
        let shared = self
            .work
            .ancestors()
            .skip(1)
            .enumerate()
            .find_map(|(up, dir)| Some((up + 1, path.strip_prefix(dir).ok()?)));
        shared.unwrap_or((depth(&self.work), path))
    }

    /// `rest` in the directory `up` levels above a workspace, written from
    /// the workspace.
    pub fn from_inside(up: usize, rest: &Path) -> PathBuf {
        let mut way: PathBuf = std::iter::repeat_n("..", up).collect();
        if !rest.as_os_str().is_empty() {
            way.push(rest);
        }
        way
    }
}

impl Root {
    /// A root at `path`, which is what it is called everywhere.
    #[cfg(test)]
    pub fn at(path: PathBuf) -> Self {
        Self { path, real: None }
    }
}

impl Roots {
    /// `hint` is any path inside the target directory, such as the
    /// compiler's output directory. `known` is the target directory if
    /// something other than Cargo's mark says where it is; it counts only
    /// when the hint is inside it.
    pub fn detect(cwd: &Path, hint: Option<&Path>, known: Option<&Path>) -> Self {
        if std::env::var_os("JATO_RELOCATE").is_some_and(|v| v == "0") {
            return Self::default();
        }
        let target = hint
            .and_then(|hint| {
                target_above(hint).or_else(|| known.filter(|known| hint.starts_with(known)))
            })
            .and_then(Root::new);
        // A workspace that is a whole filesystem or a home directory would
        // claim every path on the machine.
        let work = Root::new(cwd).filter(|_| cwd.components().count() > 2);
        Self { work, target }
    }

    /// The roots for a program that a build script started in `cwd`, the
    /// directory of the package `manifest_dir`: the workspace the package
    /// belongs to where the program runs inside it, and otherwise the
    /// directory it runs in, as for a compiler there. `out_dir` is the
    /// build script's own directory, and `known` the target directory if
    /// whoever started the build knows it.
    pub fn of_script(
        cwd: &Path,
        manifest_dir: Option<&Path>,
        out_dir: Option<&Path>,
        known: Option<&Path>,
    ) -> Self {
        let found = Self::detect(cwd, out_dir, known);
        let workspace = workspace_above(manifest_dir.unwrap_or(cwd))
            .and_then(|dir| Root::new(&dir))
            .filter(|work| work.path.components().count() > 2 && work.strip(cwd).is_some());
        Self {
            work: workspace.or(found.work),
            target: found.target,
        }
    }

    fn root(&self, id: u8) -> Option<&Root> {
        match id {
            WORK => self.work.as_ref(),
            TARGET => self.target.as_ref(),
            _ => None,
        }
    }

    /// Roots with the more specific first, so a target directory inside the
    /// workspace is not taken for part of the workspace.
    fn ordered(&self) -> impl Iterator<Item = (u8, &Root)> {
        let mut both = [
            self.work.as_ref().map(|r| (WORK, r)),
            self.target.as_ref().map(|r| (TARGET, r)),
        ];
        both.sort_by_key(|r| {
            std::cmp::Reverse(r.map_or(0, |(_, root)| root.path.as_os_str().len()))
        });
        both.into_iter().flatten()
    }

    pub fn split<'a>(&self, path: &'a Path) -> (u8, &'a Path) {
        for (id, root) in self.ordered() {
            if let Some(rest) = root.strip(path) {
                return (id, rest);
            }
        }
        (ABSOLUTE, path)
    }

    pub fn join(&self, id: u8, rest: &Path) -> Option<PathBuf> {
        if id == ABSOLUTE {
            return Some(rest.to_owned());
        }
        let root = &self.root(id)?.path;
        Some(if rest.as_os_str().is_empty() {
            root.clone()
        } else {
            root.join(rest)
        })
    }

    pub fn path(&self, id: u8) -> Option<&Path> {
        self.root(id).map(|r| r.path.as_path())
    }

    /// Every spelling of each root, to look for in a command's output.
    pub fn spellings(&self) -> Vec<(u8, &[u8])> {
        let mut all = Vec::new();
        for (id, root) in self.ordered() {
            all.push((id, root.path.as_os_str().as_bytes()));
            if let Some(real) = &root.real {
                all.push((id, real.as_os_str().as_bytes()));
            }
        }
        all
    }

    /// Replace each root in `text` by its mark.
    pub fn mark<'a>(&self, text: &'a [u8]) -> Cow<'a, [u8]> {
        let mut text = Cow::Borrowed(text);
        for (id, spelling) in self.spellings() {
            if let Some(changed) = replace_path(&text, spelling, MARKS[id as usize]) {
                text = Cow::Owned(changed);
            }
        }
        text
    }

    /// Replace each mark in `text` by the root as it is here. None if the
    /// text names a root this command does not have.
    pub fn unmark<'a>(&self, text: &'a [u8]) -> Option<Cow<'a, [u8]>> {
        if memchr::memchr(0, text).is_none() {
            return Some(Cow::Borrowed(text));
        }
        let mut out = text.to_vec();
        for id in [WORK, TARGET] {
            let mark = MARKS[id as usize];
            if memchr::memmem::find(&out, mark).is_none() {
                continue;
            }
            let root = self.root(id)?.path.as_os_str().as_bytes();
            out = replace_all(&out, mark, root);
        }
        Some(Cow::Owned(out))
    }

    pub fn mark_os<'a>(&self, text: &'a OsStr) -> Cow<'a, [u8]> {
        self.mark(text.as_bytes())
    }
}

fn replace_all(text: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = memchr::memmem::find(rest, from) {
        out.extend_from_slice(&rest[..at]);
        out.extend_from_slice(to);
        rest = &rest[at + from.len()..];
    }
    out.extend_from_slice(rest);
    out
}

/// Replace `from` where it stands for that directory, not for the start of
/// a sibling's name. None if nothing was replaced.
fn replace_path(text: &[u8], from: &[u8], to: &[u8]) -> Option<Vec<u8>> {
    let finder = memchr::memmem::Finder::new(from);
    let mut out = Vec::new();
    let mut copied = 0;
    let mut search = 0;
    let mut changed = false;
    while let Some(at) = finder.find(&text[search..]) {
        let start = search + at;
        let end = start + from.len();
        let sibling = text
            .get(end)
            .is_some_and(|b| b.is_ascii_alphanumeric() || b"._-+@~".contains(b));
        if sibling {
            search = start + 1;
            continue;
        }
        out.extend_from_slice(&text[copied..start]);
        out.extend_from_slice(to);
        copied = end;
        search = end;
        changed = true;
    }
    if !changed {
        return None;
    }
    out.extend_from_slice(&text[copied..]);
    Some(out)
}

#[cfg(test)]
mod surroundings {
    use super::*;

    fn around(work: &str, env: &[(&str, &str)]) -> Surroundings {
        let env = env
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect();
        Surroundings::of(Path::new(work), &env)
    }

    #[test]
    fn what_is_beside_a_workspace_is_told_from_what_has_a_name_of_its_own() {
        let here = around(
            "/nowhere/ben/src/olo",
            &[
                ("HOME", "/nowhere/ben"),
                (
                    "PATH",
                    "/usr/bin:/nowhere/ben/bin:/nowhere/ben/src/tools/bin",
                ),
                ("PWD", "/nowhere/ben/src/olo"),
                ("OLDPWD", "/nowhere/ben/src/other"),
            ],
        );
        // A crate next to the checkout, and the directory both are in.
        assert!(here.beside(Path::new("/nowhere/ben/src/shared/Cargo.toml")));
        assert!(here.beside(Path::new("/nowhere/ben/src/.cargo/config.toml")));
        assert!(!here.named(Path::new("/nowhere/ben/src/other/lib.rs")));
        // The person's other files and the system's are further away.
        assert!(!here.beside(Path::new("/nowhere/ben/notes.txt")));
        assert!(!here.beside(Path::new("/usr/lib64/libc.so.6")));
        // Cargo's home, and a directory PATH names, are reached by name.
        assert!(here.named(Path::new("/nowhere/ben/.cargo/registry/index")));
        assert!(here.named(Path::new("/nowhere/ben/src/tools/bin/protoc")));
        assert!(here.named(Path::new("/usr/bin/cc")));
        // A home directory the workspace is in names nothing in particular.
        assert!(!here.named(Path::new("/nowhere/ben/notes.txt")));

        // Directly in a home directory, everything else there is beside it.
        let low = around("/nowhere/ben/olo", &[("HOME", "/nowhere/ben")]);
        assert!(low.beside(Path::new("/nowhere/ben/shared/lib.rs")));
        assert!(!low.beside(Path::new("/nowhere/other/file")));
        // All but what programs keep there for the person, under names
        // that begin with a dot: those they find by the home directory.
        assert!(low.beside(Path::new("/nowhere/ben/.gitconfig")));
        assert!(low.kept(Path::new("/nowhere/ben/.gitconfig")));
        assert!(low.kept(Path::new("/nowhere/ben/.config/git/config")));
        assert!(!low.kept(Path::new("/nowhere/ben/shared/.gitconfig")));
        assert!(!low.kept(Path::new("/nowhere/ben/shared/lib.rs")));
        assert!(!low.kept(Path::new("/nowhere/ben")));
        assert!(!low.kept(Path::new("/nowhere/other/.gitconfig")));
        // Not a workspace that is itself in such a place, nor one that is
        // not in the home directory at all.
        let hidden = around("/nowhere/ben/.work/olo", &[("HOME", "/nowhere/ben")]);
        assert!(!hidden.kept(Path::new("/nowhere/ben/.work/shared/lib.rs")));
        assert!(hidden.kept(Path::new("/nowhere/ben/.gitconfig")));
        let away = around("/elsewhere/src/olo", &[("HOME", "/nowhere/ben")]);
        assert!(!away.kept(Path::new("/nowhere/ben/.gitconfig")));
        // At the top of the filesystem nothing is.
        assert!(!around("/olo", &[]).beside(Path::new("/shared/lib.rs")));
    }

    #[test]
    fn a_place_above_a_workspace_is_written_from_inside_it() {
        let here = around("/nowhere/ben/src/olo", &[]);
        for (path, up, rest) in [
            ("/nowhere/ben/src/shared/lib.rs", 1, "shared/lib.rs"),
            ("/nowhere/ben/src", 1, ""),
            ("/nowhere/.cargo/config.toml", 3, ".cargo/config.toml"),
            ("/.cargo/config.toml", 4, ".cargo/config.toml"),
            ("/usr/lib/libc.so", 4, "usr/lib/libc.so"),
        ] {
            assert_eq!(here.above(Path::new(path)), (up, Path::new(rest)), "{path}");
            let way = Surroundings::from_inside(up, Path::new(rest));
            // Followed from the workspace, the way leads back.
            let mut back = PathBuf::from("/nowhere/ben/src/olo");
            for part in way.components() {
                match part {
                    std::path::Component::ParentDir => drop(back.pop()),
                    part => back.push(part),
                }
            }
            assert_eq!(back, Path::new(path));
        }
        assert_eq!(
            Surroundings::from_inside(2, Path::new("")),
            Path::new("../..")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(work: &str, target: &str) -> Roots {
        Roots {
            work: Some(Root {
                path: work.into(),
                real: None,
            }),
            target: Some(Root {
                path: target.into(),
                real: None,
            }),
        }
    }

    #[test]
    fn text_moves_between_checkouts_and_target_directories() {
        let here = roots("/home/a/ws", "/home/a/ws/target");
        let text =
            b"/home/a/ws/target/debug/x.d: /home/a/ws/src/lib.rs /home/a/ws-old/y /home/a/ws";
        let marked = here.mark(text);
        assert!(!names(&marked, b"/home/a/ws/"));
        assert!(names(&marked, b"/home/a/ws-old/y"));
        assert_eq!(&*here.unmark(&marked).unwrap(), text);
        let there = roots("/srv/b", "/var/tmp/t");
        assert_eq!(
            &*there.unmark(&marked).unwrap(),
            b"/var/tmp/t/debug/x.d: /srv/b/src/lib.rs /home/a/ws-old/y /srv/b"
        );
        let no_target = Roots {
            target: None,
            ..there
        };
        assert!(no_target.unmark(&marked).is_none());
    }

    fn names(text: &[u8], path: &[u8]) -> bool {
        memchr::memmem::find(text, path).is_some()
    }

    #[test]
    fn paths_split_at_the_nearest_root() {
        let r = roots("/w", "/w/target");
        assert_eq!(
            r.split(Path::new("/w/src/a.rs")),
            (WORK, Path::new("src/a.rs"))
        );
        assert_eq!(
            r.split(Path::new("/w/target/debug/a")),
            (TARGET, Path::new("debug/a"))
        );
        assert_eq!(r.split(Path::new("/w")), (WORK, Path::new("")));
        assert_eq!(r.split(Path::new("/w2/x")), (ABSOLUTE, Path::new("/w2/x")));
        assert_eq!(
            r.join(TARGET, Path::new("debug/a")).unwrap(),
            Path::new("/w/target/debug/a")
        );
        assert_eq!(r.join(WORK, Path::new("")).unwrap(), Path::new("/w"));
        let inside = roots("/t/tests/trybuild/x", "/t");
        assert_eq!(
            inside.split(Path::new("/t/tests/trybuild/x/src/a.rs")),
            (WORK, Path::new("src/a.rs"))
        );
        assert_eq!(
            inside.split(Path::new("/t/debug/a")),
            (TARGET, Path::new("debug/a"))
        );
    }

    #[test]
    fn a_target_directory_is_known_by_its_mark_or_by_being_told() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (marked, bare) = (base.join("marked"), base.join("bare"));
        for target in [&marked, &bare] {
            std::fs::create_dir_all(target.join("debug/deps")).unwrap();
        }
        std::fs::write(marked.join("CACHEDIR.TAG"), "Signature: x").unwrap();
        let found = |hint: &Path, known: Option<&Path>| {
            Roots::detect(&base.join("ws"), Some(hint), known)
                .path(TARGET)
                .map(Path::to_owned)
        };
        assert_eq!(
            found(&marked.join("debug/deps"), None),
            Some(marked.clone())
        );
        assert_eq!(found(&bare.join("debug/deps"), None), None);
        assert_eq!(
            found(&bare.join("debug/deps"), Some(&bare)),
            Some(bare.clone())
        );
        // Being told of a directory the command does not write to is no
        // reason to treat it as this command's target directory.
        assert_eq!(found(&bare.join("debug/deps"), Some(&marked)), None);
        assert_eq!(
            found(&marked.join("debug/deps"), Some(&bare)),
            Some(marked.clone())
        );
    }

    #[test]
    fn paths_that_text_would_escape_are_not_roots() {
        assert!(plain(Path::new("/home/a/ws")));
        assert!(!plain(Path::new("/home/a/my project")));
        assert!(!plain(Path::new("/home/a/x\"y")));
        assert!(!plain(Path::new("/")));
        assert!(!plain(Path::new("/a/../b")));
        assert!(!plain(Path::new("relative/dir")));
    }
}
