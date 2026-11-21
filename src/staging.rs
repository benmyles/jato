//! A killed wrapper cannot run a temporary file destructor. Serialize creation
//! with cleanup, and keep an OS lock on every live named staging file.
use anyhow::Result;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
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

fn locked_file(dir: &Path) -> Result<NamedTempFile> {
    let tmp = tempfile::Builder::new().prefix("part-").tempfile_in(dir)?;
    tmp.as_file().lock()?;
    Ok(tmp)
}

pub fn file(parent: &Path) -> Result<NamedTempFile> {
    with_directory(parent, locked_file)
}

pub fn for_outputs(outputs: &[PathBuf]) -> Result<Vec<NamedTempFile>> {
    let mut files = Vec::with_capacity(outputs.len());
    for group in outputs.chunk_by(|a, b| a.parent() == b.parent()) {
        let dir = group[0]
            .parent()
            .ok_or_else(|| anyhow::anyhow!("output has no parent"))?;
        with_directory(dir, |dir| {
            for _ in group {
                files.push(locked_file(dir)?);
            }
            Ok(())
        })?;
    }
    Ok(files)
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
