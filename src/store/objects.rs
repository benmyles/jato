//! Content-addressed files share SQLite's writer lock for publication and GC.
use super::Digest;
use anyhow::Result;
use rusqlite::Transaction;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Sealed,
    Frame,
}

impl Kind {
    fn layout(self) -> (&'static str, usize) {
        match self {
            Self::Sealed => ("objects-v1", 2),
            Self::Frame => ("frames-v1", 1),
        }
    }

    pub fn path(self, dir: &Path, digest: &Digest) -> PathBuf {
        let (directory, prefix) = self.layout();
        let hex = blake3::Hash::from_bytes(*digest).to_hex();
        dir.join(directory)
            .join(&hex[..prefix])
            .join(&hex[prefix..])
    }

    pub fn collect(self, dir: &Path, tx: &Transaction<'_>, sweep: bool) -> Result<()> {
        let (directory, width) = self.layout();
        let root = dir.join(directory);
        let present = directory_exists(&root)?;
        let (retired, forget, exists) = match self {
            Self::Sealed => (
                "SELECT hash FROM retired LIMIT 128",
                "DELETE FROM retired WHERE hash=?1",
                "SELECT EXISTS(SELECT 1 FROM objects WHERE hash=?1)",
            ),
            Self::Frame => (
                "SELECT hash FROM retired_frames LIMIT 128",
                "DELETE FROM retired_frames WHERE hash=?1",
                "SELECT EXISTS(SELECT 1 FROM chunks WHERE hash=?1 AND external>0)",
            ),
        };
        let mut select = tx.prepare(retired)?;
        let mut forget = tx.prepare(forget)?;
        loop {
            let batch = select
                .query_map([], |r| r.get::<_, Digest>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if batch.is_empty() {
                break;
            }
            for digest in batch {
                let path = self.path(dir, &digest);
                if present && directory_exists(path.parent().unwrap())? {
                    remove(&path)?;
                }
                forget.execute([digest])?;
            }
        }
        if !sweep || !present {
            return Ok(());
        }
        if matches!(self, Self::Frame) {
            remove(&root.join(".pending"))?;
        }
        let mut exists = tx.prepare(exists)?;
        for shard in fs::read_dir(root)? {
            let shard = shard?;
            let prefix = shard.file_name();
            let Some(prefix) = prefix.to_str() else {
                continue;
            };
            if prefix.len() != width || !shard.file_type()?.is_dir() {
                continue;
            }
            for entry in fs::read_dir(shard.path())? {
                let entry = entry?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                let Ok(hash) = blake3::Hash::from_hex(format!("{prefix}{name}")) else {
                    continue;
                };
                if !exists.query_row([hash.as_bytes()], |r| r.get::<_, bool>(0))?
                    && entry.file_type()?.is_file()
                {
                    fs::remove_file(entry.path())?;
                }
            }
            let _ = fs::remove_dir(shard.path());
        }
        Ok(())
    }
}

fn directory_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => Ok(true),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "redirected cache directory",
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

// Called under the writer lock. The configured cache root itself may be a
// symlink. The two managed components below it must be directories we own.
pub(crate) fn prepare_shard(shard: &Path) -> io::Result<()> {
    for path in [shard.parent().expect("object namespace"), shard] {
        match fs::create_dir(path) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && directory_exists(path)? => (),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub(super) fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}
