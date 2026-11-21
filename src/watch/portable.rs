use super::{Changes, Inbox};
use anyhow::Result;
use notify::event::{AccessKind, AccessMode, ModifyKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::ffi::OsStr;
use std::os::fd::RawFd;
use std::path::Path;
use std::sync::Arc;
pub struct Events(RecommendedWatcher);
impl Events {
    pub fn new(inbox: Arc<Inbox>) -> Result<Self> {
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
        Ok(Self(watcher))
    }
    pub fn watch(&mut self, path: Arc<Path>) -> Result<()> {
        self.0.watch(&path, RecursiveMode::NonRecursive)?;
        Ok(())
    }
    pub fn fd(&self) -> RawFd {
        -1
    }
    pub fn read(&mut self, inbox: &Inbox) -> Result<Changes> {
        Ok(std::mem::take(&mut *inbox.changes.lock().unwrap()))
    }
}
