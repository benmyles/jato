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

#[cfg(target_os = "linux")]
pub fn anonymous(parent: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_TMPFILE)
        .open(parent)
}

#[cfg(target_os = "linux")]
pub fn link_file(file: &File, path: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::{fd::AsRawFd, unix::ffi::OsStrExt};
    let from = CString::new(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
    let to = CString::new(path.as_os_str().as_bytes())?;
    let r = unsafe {
        libc::linkat(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::AT_SYMLINK_FOLLOW,
        )
    };
    if r < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub enum Output {
    #[cfg(target_os = "linux")]
    Anonymous(File),
    Named(NamedTempFile),
}

impl Output {
    pub fn writer(&mut self) -> &mut File {
        match self {
            #[cfg(target_os = "linux")]
            Self::Anonymous(f) => f,
            Self::Named(f) => f.as_file_mut(),
        }
    }
    pub fn publish(self, path: &Path) -> Result<()> {
        match self {
            Self::Named(f) => {
                f.persist(path)?;
            }
            #[cfg(target_os = "linux")]
            Self::Anonymous(mut f) => {
                use std::io::Seek;
                match link_file(&f, path) {
                    Ok(()) => return Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        // Link under a temporary name, then atomically replace
                        // the previous artifact, preserving open readers.
                        f.lock()?;
                        let parent = path
                            .parent()
                            .ok_or_else(|| anyhow::anyhow!("output has no parent"))?;
                        let named = with_directory(parent, |dir| {
                            Ok(tempfile::Builder::new()
                                .prefix("part-")
                                .make_in(dir, |slot| {
                                    let lease = f.try_clone()?;
                                    link_file(&f, slot)?;
                                    Ok(lease)
                                })?)
                        })?;
                        named.persist(path)?;
                        return Ok(());
                    }
                    Err(_) => (),
                }
                // Some mounts can create O_TMPFILE but cannot publish its fd.
                let mut named = file(
                    path.parent()
                        .ok_or_else(|| anyhow::anyhow!("output has no parent"))?,
                )?;
                f.rewind()?;
                std::io::copy(&mut f, named.as_file_mut())?;
                named
                    .as_file()
                    .set_permissions(f.metadata()?.permissions())?;
                named.persist(path)?;
            }
        }
        Ok(())
    }
}

pub fn for_outputs(outputs: &[PathBuf]) -> Result<Vec<Output>> {
    let mut files = Vec::with_capacity(outputs.len());
    for group in outputs.chunk_by(|a, b| a.parent() == b.parent()) {
        let dir = group[0]
            .parent()
            .ok_or_else(|| anyhow::anyhow!("output has no parent"))?;
        #[cfg(target_os = "linux")]
        if let Ok(first) = anonymous(dir) {
            files.push(Output::Anonymous(first));
            for _ in &group[1..] {
                files.push(Output::Anonymous(anonymous(dir)?));
            }
            continue;
        }
        with_directory(dir, |dir| {
            for _ in group {
                files.push(Output::Named(locked_file(dir)?));
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
