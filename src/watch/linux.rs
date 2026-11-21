//! The main watch loop polls the inotify queue alongside signals and Cargo's
//! completion fd. Registration and event handling use the same descriptor.
use super::{Changes, Inbox};
use anyhow::Result;
use inotify::{EventMask, Inotify, WatchMask};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;
use std::sync::Arc;

pub struct Events {
    queue: Inotify,
    directories: HashMap<i32, Arc<Path>>,
    // Bind mounts can expose one watched inode through different directories.
    aliases: HashMap<i32, Vec<Arc<Path>>>,
    buffer: Box<[u8]>,
}

impl Events {
    pub fn new(_: Arc<Inbox>) -> Result<Self> {
        Ok(Self {
            queue: Inotify::init()?,
            directories: HashMap::new(),
            aliases: HashMap::new(),
            buffer: vec![0; 32 * 1024].into_boxed_slice(),
        })
    }

    pub fn watch(&mut self, path: Arc<Path>) -> Result<()> {
        let mask = WatchMask::ONLYDIR
            | WatchMask::CREATE
            | WatchMask::DELETE
            | WatchMask::MODIFY
            | WatchMask::ATTRIB
            | WatchMask::MOVED_FROM
            | WatchMask::MOVED_TO
            | WatchMask::MOVE_SELF
            | WatchMask::DELETE_SELF
            | WatchMask::CLOSE_WRITE;
        let id = self
            .queue
            .watches()
            .add(&*path, mask)?
            .get_watch_descriptor_id();
        match self.directories.entry(id) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(path);
            }
            std::collections::hash_map::Entry::Occupied(_) => {
                self.aliases.entry(id).or_default().push(path);
            }
        }
        Ok(())
    }

    pub fn fd(&self) -> RawFd {
        self.queue.as_raw_fd()
    }

    pub fn read(&mut self, _: &Inbox) -> Result<Changes> {
        let mut changes = Changes::default();
        // A sustained event storm must also let the main loop service Ctrl-C
        // and child completion. poll will wake again if more records remain.
        for _ in 0..8 {
            let events = match self.queue.read_events(&mut self.buffer) {
                Ok(events) => events,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(changes),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            for event in events {
                if changes.full {
                    continue;
                }
                let id = event.wd.get_watch_descriptor_id();
                let Some(dir) = self.directories.get(&id) else {
                    changes = Changes::all();
                    continue;
                };
                if event
                    .mask
                    .intersects(EventMask::Q_OVERFLOW | EventMask::UNMOUNT)
                {
                    changes = Changes::all();
                    continue;
                }
                let close = event.mask.contains(EventMask::CLOSE_WRITE);
                let moved = event.mask.contains(EventMask::MOVED_TO);
                let created = event.mask.contains(EventMask::CREATE | EventMask::ISDIR);
                for dir in std::iter::once(dir).chain(self.aliases.get(&id).into_iter().flatten()) {
                    let path = event
                        .name
                        .map_or_else(|| dir.to_path_buf(), |name| dir.join(name));
                    if close {
                        if path.extension() == Some(OsStr::new("rs")) {
                            changes.completed = Some(path);
                        }
                    } else {
                        let completed = (moved && path.extension() == Some(OsStr::new("rs")))
                            .then(|| path.clone());
                        changes.add(path, created);
                        if completed.is_some() {
                            changes.completed = completed;
                        }
                    }
                }
            }
        }
        // There may be unread records when this descriptor is replaced.
        // Preserve coverage through an overflow or a long burst.
        Ok(Changes::all())
    }
}
