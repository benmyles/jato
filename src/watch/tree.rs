use super::workspace::Workspace;
use super::{Changes, Inbox};
use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::event::{AccessKind, AccessMode, ModifyKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Tree {
    watcher: RecommendedWatcher,
    directories: BTreeSet<PathBuf>,
    ignores: BTreeMap<PathBuf, Gitignore>,
    roots: BTreeSet<PathBuf>,
    excluded: Vec<PathBuf>,
}

impl Tree {
    pub fn new(workspace: &Workspace, inbox: Arc<Inbox>) -> Result<Self> {
        let watcher =
            notify::recommended_watcher(move |event: notify::Result<Event>| match event {
                Ok(e) if !e.need_rescan() && !e.paths.is_empty() => {
                    let closed = matches!(
                        e.kind,
                        EventKind::Access(AccessKind::Close(AccessMode::Write))
                    );
                    let renamed = matches!(
                        e.kind,
                        EventKind::Modify(ModifyKind::Name(RenameMode::To | RenameMode::Both))
                    );
                    let completed = (closed || renamed)
                        .then(|| e.paths.last())
                        .flatten()
                        .filter(|p| p.extension() == Some(OsStr::new("rs")))
                        .cloned();
                    if closed {
                        // A close is a completion hint for a pending change.
                        // Opening a file for writing without editing it, or
                        // closing after a prior run, must not start another job.
                        if completed.is_some() {
                            inbox.record(Vec::new(), false, completed);
                        }
                    } else if !matches!(e.kind, EventKind::Access(_)) {
                        let directory =
                            matches!(e.kind, EventKind::Create(notify::event::CreateKind::Folder));
                        inbox.record(e.paths, directory, completed);
                    }
                }
                _ => inbox.rescan(),
            })?;
        let mut tree = Self {
            watcher,
            directories: BTreeSet::new(),
            ignores: BTreeMap::new(),
            roots: workspace.roots.clone(),
            excluded: workspace.excluded.clone(),
        };
        for root in &workspace.roots {
            if root.is_dir() {
                tree.add_dir(root)?;
            } else if let Some(parent) = root.parent() {
                tree.watch_directory(parent)?;
            }
        }
        Ok(tree)
    }

    fn watch_directory(&mut self, path: &Path) -> Result<()> {
        if self.directories.insert(path.to_owned()) {
            self.watcher
                .watch(path, RecursiveMode::NonRecursive)
                .with_context(|| format!("watching {}", path.display()))?;
        }
        Ok(())
    }

    fn add_dir(&mut self, root: &Path) -> Result<()> {
        let mut todo = vec![root.to_owned()];
        while let Some(dir) = todo.pop() {
            if self.ignores.contains_key(&dir) || self.ignored(&dir, true) {
                continue;
            }
            self.watch_directory(&dir)?;
            let mut builder = GitignoreBuilder::new(&dir);
            for name in [".git/info/exclude", ".gitignore", ".ignore"] {
                let path = dir.join(name);
                if path.is_file() {
                    builder.add(path);
                }
            }
            self.ignores.insert(dir.clone(), builder.build()?);
            for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    todo.push(entry.path());
                }
            }
        }
        Ok(())
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

    pub fn filter(&self, mut changes: Changes) -> Changes {
        changes.paths.retain(|p| !self.ignored(p, p.is_dir()));
        changes
    }

    pub fn drop_transient_output_dirs(&self, changes: &mut Changes) {
        // Cargo initializes an output directory by making a temporary sibling
        // named <basename><six random characters>, then renaming it. Ignore
        // only a newly created directory that has already disappeared.
        changes.paths.retain(|p| {
            if !changes.new_dirs.contains(p) || self.directories.contains(p) || p.exists() {
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
            .any(|p| self.directories.contains(p) || p.is_dir())
    }
}
