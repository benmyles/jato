//! A successful chdir to the original absolute pathname is a no-op only if
//! that name kept referring to the original directory throughout observation.
use anyhow::{Result, ensure};
use inotify::{Inotify, WatchMask};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

pub(super) struct Guard(Inotify);

impl Guard {
    pub fn current(path: &Path) -> Result<Self> {
        Self::watch(path, fs::metadata(".")?)
    }

    fn watch(path: &Path, before: fs::Metadata) -> Result<Self> {
        ensure!(
            fs::canonicalize(path)? == path,
            "redirected compiler directory"
        );
        let mut guard = Self(Inotify::init()?);
        for dir in path.ancestors() {
            guard.0.watches().add(
                dir,
                WatchMask::ONLYDIR
                    | WatchMask::DONT_FOLLOW
                    | WatchMask::MOVE_SELF
                    | WatchMask::DELETE_SELF,
            )?;
        }
        let after = fs::metadata(path)?;
        ensure!(
            before.dev() == after.dev() && before.ino() == after.ino(),
            "compiler directory replaced"
        );
        guard.check()?;
        Ok(guard)
    }

    pub fn check(&mut self) -> Result<()> {
        match self.0.read_events(&mut [0; 1024]) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(e) => Err(e.into()),
            Ok(mut events) => {
                ensure!(
                    events.next().is_none(),
                    "compiler directory moved during observation"
                );
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detects_an_ancestor_moved_away_and_back() -> Result<()> {
        let root = tempfile::tempdir()?;
        let parent = root.path().join("parent");
        let dir = parent.join("project");
        fs::create_dir_all(&dir)?;
        let mut guard = Guard::watch(&fs::canonicalize(&dir)?, fs::metadata(&dir)?)?;
        fs::write(parent.join("unrelated"), "edit")?;
        guard.check()?;
        let moved = root.path().join("elsewhere");
        fs::rename(&parent, &moved)?;
        fs::rename(&moved, &parent)?;
        assert!(guard.check().is_err());
        Ok(())
    }
}
