//! A killed wrapper cannot run a temporary file destructor. Serialize creation
//! with cleanup, and keep an OS lock on every live named staging file.
use anyhow::Result;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use tempfile::NamedTempFile;

fn with_directory<T>(parent: &Path, action: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
    let dir = parent.join(".jato-staging-v1");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    let gate = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("lock"))?;
    gate.lock()?;
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.file_name().as_encoded_bytes().starts_with(b"part-")
            && entry.file_type()?.is_file()
            && let Ok(file) = File::open(entry.path())
            && file.try_lock().is_ok()
        {
            let _ = fs::remove_file(entry.path());
        }
    }
    action(&dir)
}

pub fn file(parent: &Path) -> Result<NamedTempFile> {
    with_directory(parent, |dir| {
        let tmp = tempfile::Builder::new().prefix("part-").tempfile_in(dir)?;
        tmp.as_file().lock()?;
        Ok(tmp)
    })
}

pub fn clean(parent: &Path) -> Result<()> {
    if parent.join(".jato-staging-v1").exists() {
        with_directory(parent, |_| Ok(()))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_abandoned_staging_files_are_removed() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let (lease, path) = file(dir.path())?.keep()?;
        clean(dir.path())?;
        assert!(path.exists());
        drop(lease);
        clean(dir.path())?;
        assert!(!path.exists());
        Ok(())
    }
}
