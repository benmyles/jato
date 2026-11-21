use anyhow::Result;
use anyhow::{Context, ensure};
use std::ffi::OsStr;
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::Path;
use std::path::PathBuf;

// Upstream mold 3.0.0, x86_64 Linux. The release archive's SHA-256 is
// 6c90d4a474c7c0409dfb575be03a5345878ac14fdba18de8b40fa58c60121189.
// The file length is only a negative filter. Admission checks the whole ELF.
const BYTES: u64 = 42_246_008;
const BLAKE3: &str = "81174f3352cee00d4bf323b8f3f56eab3543877c9f0211f66f7d6620961c3b7b";
// SHA-256 fs-verity descriptor for the same upstream ELF, 4096-byte
// blocks and no salt. Both pins describe the identical file contents.
const VERITY: &str = "87faf70327bf78a89dda395a5135bc0d73750f967dedcabb4e6450814229e544";

#[derive(Clone, Copy)]
pub(super) enum InputDigest {
    Read(usize),
    SealedMold,
}

impl InputDigest {
    pub fn value(self, digests: &[crate::store::Digest]) -> crate::store::Digest {
        match self {
            Self::Read(index) => digests[index],
            Self::SealedMold => *blake3::Hash::from_hex(BLAKE3)
                .expect("pinned release digest")
                .as_bytes(),
        }
    }
}

pub(super) fn sealed(path: &Path, real: &Path, meta: &std::fs::Metadata) -> Result<bool> {
    use crate::fingerprint::stamp;
    if meta.len() != BYTES
        || !matches!(
            real.file_name().and_then(OsStr::to_str),
            Some("mold" | "ld.mold")
        )
    {
        return Ok(false);
    }
    let file = crate::content::open(path)?;
    ensure!(
        stamp(&file.metadata()?) == stamp(meta),
        "linker changed before measurement"
    );
    let matches = crate::sealed::measure(&file)
        .is_ok_and(|v| blake3::Hash::from_bytes(v).to_hex().as_str() == VERITY);
    ensure!(
        stamp(&std::fs::metadata(path)?) == stamp(meta),
        "linker changed during measurement"
    );
    Ok(matches)
}

pub(super) fn candidate(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(OsStr::to_str),
        Some("mold" | "ld.mold")
    ) && path
        .symlink_metadata()
        .is_ok_and(|m| m.is_file() && m.len() == BYTES)
}

pub(super) fn authentic(digest: &crate::store::Digest) -> bool {
    blake3::Hash::from_bytes(*digest).to_hex().as_str() == BLAKE3
}

pub(super) struct Output {
    final_path: PathBuf,
    candidate: Option<(u32, PathBuf)>,
    published: bool,
}

impl Output {
    pub fn new(path: PathBuf) -> Option<Self> {
        // On a fresh link mold first tries to move an old output aside. Reject
        // existing PID-shaped siblings before tracing a non-exclusive create.
        if !path
            .symlink_metadata()
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            return None;
        }
        let name = path.file_name()?.as_bytes();
        for (i, file) in std::fs::read_dir(path.parent()?).ok()?.enumerate() {
            let filename = file.ok()?.file_name();
            if i >= 16384
                || filename
                    .as_bytes()
                    .strip_prefix(b".")
                    .and_then(|rest| rest.strip_prefix(name))
                    .and_then(|rest| rest.strip_prefix(b"."))
                    .is_some_and(|id| !id.is_empty() && id.iter().all(u8::is_ascii_digit))
            {
                return None;
            }
        }
        Some(Self {
            final_path: path,
            candidate: None,
            published: false,
        })
    }

    pub fn observe(
        &mut self,
        pid: u32,
        name: &str,
        args: &str,
        result: &str,
        cwd: &Path,
    ) -> Result<()> {
        if matches!(name, "open" | "openat") && args.contains("O_TRUNC") {
            let (path, rest) = super::argument(args, name == "openat", cwd)?;
            if path.parent() != self.final_path.parent() {
                return Ok(());
            }
            let mut tmp = std::ffi::OsString::from(".");
            tmp.push(
                self.final_path
                    .file_name()
                    .context("missing linker output name")?,
            );
            tmp.push(format!(".{pid}"));
            if path.file_name() == Some(tmp.as_os_str()) {
                let flags = rest.strip_prefix(", ").and_then(|s| s.split(',').next());
                ensure!(
                    self.candidate.is_none()
                        && flags == Some("O_RDWR|O_CREAT|O_TRUNC|O_CLOEXEC")
                        && super::fd_path(result)? == path,
                    "unexpected intermediate linker open"
                );
                self.candidate = Some((pid, path));
            }
        } else if name == "rename"
            && let Some((owner, tmp)) = &self.candidate
        {
            let (source, rest) = super::argument(args, false, cwd)?;
            let (dest, tail) = super::argument(
                rest.strip_prefix(", ")
                    .context("missing rename destination")?,
                false,
                cwd,
            )?;
            ensure!(
                dest != *tmp || result != "0",
                "replaced linker intermediate"
            );
            if source == *tmp {
                ensure!(
                    pid == *owner
                        && !self.published
                        && tail.is_empty()
                        && dest == self.final_path
                        && result == "0",
                    "unexpected intermediate linker rename"
                );
                self.published = true;
            }
        }
        Ok(())
    }

    pub fn temporary(&self) -> Option<&Path> {
        let (_, tmp) = self.candidate.as_ref()?;
        let output = self.final_path.symlink_metadata().ok()?;
        (self.published
            && output.is_file()
            && output.nlink() == 1
            && tmp
                .symlink_metadata()
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound))
        .then_some(tmp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::FileExt;

    #[test]
    fn a_pid_shaped_sibling_or_linked_result_cannot_certify_a_temp() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let output = dir.path().join("program");
        let temp = dir.path().join(".program.42");
        fs::write(&temp, "unrelated")?;
        ensure!(Output::new(output.clone()).is_none());
        fs::remove_file(&temp)?;
        let mut observed = Output::new(output.clone()).unwrap();
        observed.candidate = Some((42, temp.clone()));
        fs::write(&output, "program")?;
        assert!(observed.temporary().is_none());
        observed.published = true;
        assert_eq!(observed.temporary(), Some(temp.as_path()));
        fs::hard_link(&output, dir.path().join("other"))?;
        assert!(observed.temporary().is_none());
        Ok(())
    }

    #[test]
    fn release_size_cannot_authenticate_modified_mold() -> Result<()> {
        let Some(upstream) = std::env::var_os("JATO_TEST_MOLD") else {
            return Ok(());
        };
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("mold");
        fs::copy(upstream, &path)?;
        let manifest = super::super::Manifest {
            verified_mold: true,
            release_images: [path.clone()].into(),
            read: vec![path.clone()],
            lookup: vec![],
            missing: vec![],
            listed: vec![],
            shared_listing: None,
        };
        let digest = || {
            manifest.digest(
                Path::new("/unused"),
                std::time::SystemTime::now(),
                &Default::default(),
                &[],
            )
        };
        digest()?;
        #[cfg(target_os = "linux")]
        if std::env::var_os("JATO_TEST_SEALED").is_some() {
            let original = digest()?;
            ensure!(!sealed(&path, &path, &path.metadata()?)?);
            crate::sealed::seal_fixture(&path)?;
            ensure!(sealed(&path, &path, &path.metadata()?)?);
            assert_eq!(digest()?, original);
            // The private fixture alone is immutable. Replace its name with a
            // writable copy, as a package manager would during an upgrade.
            let replacement = dir.path().join("replacement");
            fs::copy(&path, &replacement)?;
            fs::rename(replacement, &path)?;
        }
        let file = fs::OpenOptions::new().read(true).write(true).open(&path)?;
        let mut byte = [0];
        file.read_exact_at(&mut byte, BYTES / 2)?;
        byte[0] ^= 1;
        file.write_all_at(&byte, BYTES / 2)?;
        drop(file);
        assert!(candidate(&path));
        assert!(digest().is_err());
        #[cfg(target_os = "linux")]
        if std::env::var_os("JATO_TEST_SEALED").is_some() {
            crate::sealed::seal_fixture(&path)?;
            ensure!(!sealed(&path, &path, &path.metadata()?)?);
            assert!(digest().is_err());
        }
        Ok(())
    }
}
