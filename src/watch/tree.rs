use super::workspace::Workspace;
use super::{Changes, Inbox, events::Events};
use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Tree {
    watcher: Events,
    directories: HashSet<Arc<Path>>,
    ignores: BTreeMap<PathBuf, Gitignore>,
    roots: BTreeSet<PathBuf>,
    excluded: Vec<PathBuf>,
}

impl Tree {
    pub fn new(workspace: &Workspace, inbox: Arc<Inbox>) -> Result<Self> {
        let watcher = Events::new(inbox)?;
        let mut tree = Self {
            watcher,
            directories: HashSet::new(),
            ignores: BTreeMap::new(),
            roots: workspace.roots.clone(),
            excluded: workspace.excluded.clone(),
        };
        // Complete recursive roots first. A single-file watch only registers
        // its parent and must not mark an unvisited subtree as scanned.
        let mut parents = BTreeSet::new();
        for root in &workspace.roots {
            if root.is_dir() {
                tree.add_dir(root)?;
            } else if let Some(parent) = root.parent() {
                parents.insert(parent);
            }
        }
        for parent in parents {
            tree.watch_directory(parent)?;
        }
        Ok(tree)
    }

    fn watch_directory(&mut self, path: &Path) -> Result<bool> {
        if self.directories.contains(path) {
            return Ok(false);
        }
        let path: Arc<Path> = path.into();
        self.watcher
            .watch(path.clone())
            .with_context(|| format!("watching {}", path.display()))?;
        self.directories.insert(path);
        Ok(true)
    }

    fn add_dir(&mut self, root: &Path) -> Result<()> {
        let mut todo = vec![root.to_owned()];
        while let Some(dir) = todo.pop() {
            if self.ignored(&dir, true) || !self.watch_directory(&dir)? {
                continue;
            }
            // Watch before enumerating. Find ignore files in the same listing
            // as children; most directories need no matcher or metadata probes.
            let mut present = [false; 3];
            for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
                let entry = entry?;
                match entry.file_name().as_encoded_bytes() {
                    b".git" => present[0] = true,
                    b".gitignore" => present[1] = true,
                    b".ignore" => present[2] = true,
                    _ => (),
                }
                if entry.file_type()?.is_dir() {
                    todo.push(entry.path());
                }
            }
            if present.contains(&true) {
                let mut builder = GitignoreBuilder::new(&dir);
                for (name, found) in [".git/info/exclude", ".gitignore", ".ignore"]
                    .into_iter()
                    .zip(present)
                {
                    if found {
                        let path = dir.join(name);
                        if path.is_file() {
                            builder.add(path);
                        }
                    }
                }
                let matcher = builder.build()?;
                if !matcher.is_empty() {
                    self.ignores.insert(dir, matcher);
                }
            }
        }
        Ok(())
    }

    /// Cargo has re-resolved workspace ownership. With unchanged roots, a
    /// newly introduced subtree can join the running watcher in place.
    pub fn extend(&mut self, workspace: &Workspace, paths: &BTreeSet<PathBuf>) -> Result<bool> {
        if self.roots != workspace.roots
            || self.excluded != workspace.excluded
            || paths.iter().any(|p| self.directories.contains(p.as_path()))
        {
            return Ok(false);
        }
        for path in paths {
            if fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
                self.add_dir(path)?;
            }
        }
        Ok(true)
    }

    fn ignored(&self, path: &Path, dir: bool) -> bool {
        if self.excluded.iter().any(|p| path.starts_with(p))
            || path
                .components()
                .any(|c| matches!(c.as_os_str().to_str(), Some(".git" | ".hg" | ".jj")))
        {
            return true;
        }
        let Some(root) = path.ancestors().find(|p| self.roots.contains(*p)) else {
            return true;
        };
        if root == path {
            return false;
        }
        if !dir
            && path.file_name().and_then(OsStr::to_str).is_some_and(|n| {
                n.ends_with('~')
                    || n.ends_with(".swp")
                    || n.ends_with(".swx")
                    || n.starts_with(".#")
            })
        {
            return true;
        }
        if self.ignores.is_empty() {
            return false;
        }
        for parent in path.ancestors().skip(1) {
            if let Some(ignore) = self.ignores.get(parent) {
                let matched = ignore.matched_path_or_any_parents(path, dir);
                if !matched.is_none() {
                    return matched.is_ignore();
                }
            }
            if parent == root {
                break;
            }
        }
        false
    }

    pub fn fd(&self) -> RawFd {
        self.watcher.fd()
    }

    pub fn changes(&mut self, inbox: &Inbox) -> Result<Changes> {
        let mut changes = self.watcher.read(inbox)?;
        changes.paths.retain(|p| !self.ignored(p, p.is_dir()));
        Ok(changes)
    }

    pub fn drop_transient_output_dirs(&self, changes: &mut Changes) {
        // Cargo initializes an output directory by making a temporary sibling
        // named <basename><six random characters>, then renaming it. Ignore
        // only a newly created directory that has already disappeared.
        changes.paths.retain(|p| {
            if !changes.new_dirs.contains(p) || self.directories.contains(p.as_path()) || p.exists()
            {
                return true;
            }
            let name = p.file_name().unwrap_or_default().as_encoded_bytes();
            !self.excluded.iter().any(|out| {
                out.parent() == p.parent()
                    && out.file_name().is_some_and(|base| {
                        name.strip_prefix(base.as_encoded_bytes()).is_some_and(|s| {
                            s.len() == 6 && s.iter().all(u8::is_ascii_alphanumeric)
                        })
                    })
            })
        });
    }

    pub fn topology_changed(&self, paths: &BTreeSet<PathBuf>) -> bool {
        paths
            .iter()
            .any(|p| self.directories.contains(p.as_path()) || p.is_dir())
    }
}
