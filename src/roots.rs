//! The two directories whose location must not matter: the directory Cargo
//! runs the compiler in, and the target directory.
//!
//! Paths beneath them are stored relative to them, so a result recorded in
//! one checkout or target directory is found from another. Whether a result
//! can really be used elsewhere is decided separately, from its bytes.
use std::borrow::Cow;
use std::ffi::OsStr;
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

impl Roots {
    /// `hint` is any path inside the target directory, such as the
    /// compiler's output directory.
    pub fn detect(cwd: &Path, hint: Option<&Path>) -> Self {
        if std::env::var_os("JATO_RELOCATE").is_some_and(|v| v == "0") {
            return Self::default();
        }
        let target = hint.and_then(target_above).and_then(Root::new);
        // A workspace that is a whole filesystem or a home directory would
        // claim every path on the machine.
        let work = Root::new(cwd).filter(|_| cwd.components().count() > 2);
        Self { work, target }
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
    fn paths_that_text_would_escape_are_not_roots() {
        assert!(plain(Path::new("/home/a/ws")));
        assert!(!plain(Path::new("/home/a/my project")));
        assert!(!plain(Path::new("/home/a/x\"y")));
        assert!(!plain(Path::new("/")));
        assert!(!plain(Path::new("/a/../b")));
        assert!(!plain(Path::new("relative/dir")));
    }
}
